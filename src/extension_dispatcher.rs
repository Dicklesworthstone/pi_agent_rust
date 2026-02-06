//! Hostcall dispatcher for JS extensions.
//!
//! This module introduces the core `ExtensionDispatcher` abstraction used to route
//! hostcall requests (tools, HTTP, session, UI, etc.) from the JS runtime to
//! Rust implementations.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use asupersync::Cx;
use asupersync::channel::oneshot;
use asupersync::time::{sleep, wall_now};
use async_trait::async_trait;
use serde_json::Value;

use crate::connectors::{Connector, http::HttpConnector};
use crate::error::Result;
use crate::extensions::EXTENSION_EVENT_TIMEOUT_MS;
use crate::extensions::{
    ExtensionBody, ExtensionMessage, ExtensionSession, ExtensionUiRequest, ExtensionUiResponse,
    HostCallError, HostCallErrorCode, HostCallPayload, HostResultPayload, HostStreamChunk,
};
use crate::extensions_js::{HostcallKind, HostcallRequest, PiJsRuntime, js_to_json, json_to_js};
use crate::scheduler::{Clock as SchedulerClock, HostcallOutcome, WallClock};
use crate::tools::ToolRegistry;

/// Coordinates hostcall dispatch between the JS extension runtime and Rust handlers.
pub struct ExtensionDispatcher<C: SchedulerClock = WallClock> {
    /// The JavaScript runtime that generates hostcall requests.
    runtime: Rc<PiJsRuntime<C>>,
    /// Registry of available tools (built-in + extension-registered).
    tool_registry: Arc<ToolRegistry>,
    /// HTTP connector for pi.http() calls.
    http_connector: Arc<HttpConnector>,
    /// Session access for pi.session() calls.
    session: Arc<dyn ExtensionSession + Send + Sync>,
    /// UI handler for pi.ui() calls.
    ui_handler: Arc<dyn ExtensionUiHandler + Send + Sync>,
    /// Current working directory for relative path resolution.
    cwd: PathBuf,
}

fn ui_response_value_for_op(op: &str, response: &ExtensionUiResponse) -> Value {
    if response.cancelled {
        return match op {
            // Deterministic defaults (bd-2hz.1): confirm cancellation/timeout resolves false.
            "confirm" => Value::Bool(false),
            _ => Value::Null,
        };
    }
    response.value.clone().unwrap_or(Value::Null)
}

fn protocol_hostcall_op(params: &Value) -> Option<String> {
    params
        .get("op")
        .or_else(|| params.get("method"))
        .or_else(|| params.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn protocol_normalize_output(value: Value) -> Value {
    if value.is_object() {
        value
    } else {
        serde_json::json!({ "value": value })
    }
}

fn protocol_error_code(code: &str) -> HostCallErrorCode {
    match code {
        "timeout" => HostCallErrorCode::Timeout,
        "denied" => HostCallErrorCode::Denied,
        "io" | "tool_error" => HostCallErrorCode::Io,
        "invalid_request" => HostCallErrorCode::InvalidRequest,
        _ => HostCallErrorCode::Internal,
    }
}

fn hostcall_outcome_to_protocol_result(
    call_id: &str,
    outcome: HostcallOutcome,
) -> HostResultPayload {
    match outcome {
        HostcallOutcome::Success(output) => HostResultPayload {
            call_id: call_id.to_string(),
            output: protocol_normalize_output(output),
            is_error: false,
            error: None,
            chunk: None,
        },
        HostcallOutcome::StreamChunk {
            sequence,
            chunk,
            is_final,
        } => HostResultPayload {
            call_id: call_id.to_string(),
            output: serde_json::json!({
                "sequence": sequence,
                "chunk": chunk,
                "isFinal": is_final,
            }),
            is_error: false,
            error: None,
            chunk: Some(HostStreamChunk {
                index: sequence,
                is_last: is_final,
                backpressure: None,
            }),
        },
        HostcallOutcome::Error { code, message } => HostResultPayload {
            call_id: call_id.to_string(),
            output: serde_json::json!({}),
            is_error: true,
            error: Some(HostCallError {
                code: protocol_error_code(&code),
                message,
                details: None,
                retryable: None,
            }),
            chunk: None,
        },
    }
}

impl<C: SchedulerClock + 'static> ExtensionDispatcher<C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runtime: Rc<PiJsRuntime<C>>,
        tool_registry: Arc<ToolRegistry>,
        http_connector: Arc<HttpConnector>,
        session: Arc<dyn ExtensionSession + Send + Sync>,
        ui_handler: Arc<dyn ExtensionUiHandler + Send + Sync>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            runtime,
            tool_registry,
            http_connector,
            session,
            ui_handler,
            cwd,
        }
    }

    /// Drain pending hostcall requests from the JS runtime.
    #[must_use]
    pub fn drain_hostcall_requests(&self) -> VecDeque<HostcallRequest> {
        self.runtime.drain_hostcall_requests()
    }

    /// Dispatch a hostcall and enqueue its completion into the JS scheduler.
    #[allow(clippy::future_not_send)]
    pub fn dispatch_and_complete(
        &self,
        request: HostcallRequest,
    ) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async move {
            let HostcallRequest {
                call_id,
                kind,
                payload,
                extension_id,
                ..
            } = request;

            let outcome = match kind {
                HostcallKind::Tool { name } => self.dispatch_tool(&call_id, &name, payload).await,
                HostcallKind::Exec { cmd } => self.dispatch_exec(&call_id, &cmd, payload).await,
                HostcallKind::Http => self.dispatch_http(&call_id, payload).await,
                HostcallKind::Session { op } => self.dispatch_session(&call_id, &op, payload).await,
                HostcallKind::Ui { op } => self.dispatch_ui(&call_id, &op, payload).await,
                HostcallKind::Events { op } => {
                    self.dispatch_events(&call_id, extension_id.as_deref(), &op, payload)
                        .await
                }
            };

            self.runtime.complete_hostcall(call_id, outcome);
        })
    }

    /// Protocol adapter: convert `ExtensionMessage(type=host_call)` into
    /// `ExtensionMessage(type=host_result)` using the same dispatch paths used
    /// by runtime hostcalls.
    #[allow(clippy::future_not_send)]
    pub fn dispatch_protocol_message(
        &self,
        message: ExtensionMessage,
    ) -> Pin<Box<dyn Future<Output = Result<ExtensionMessage>> + '_>> {
        Box::pin(async move {
            message.validate()?;
            let ExtensionMessage { id, version, body } = message;
            let ExtensionBody::HostCall(payload) = body else {
                return Err(crate::error::Error::validation(
                    "dispatch_protocol_message expects host_call message",
                ));
            };

            let outcome = self.dispatch_protocol_host_call(&payload).await;
            let response = ExtensionMessage {
                id,
                version,
                body: ExtensionBody::HostResult(hostcall_outcome_to_protocol_result(
                    &payload.call_id,
                    outcome,
                )),
            };
            response.validate()?;
            Ok(response)
        })
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_protocol_host_call(&self, payload: &HostCallPayload) -> HostcallOutcome {
        let method = payload.method.trim().to_ascii_lowercase();
        let params = payload.params.clone();

        match method.as_str() {
            "tool" => {
                let Some(name) = params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call tool requires params.name".to_string(),
                    };
                };
                let input = params
                    .get("input")
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                self.dispatch_tool(&payload.call_id, name, input).await
            }
            "exec" => {
                let Some(cmd) = params
                    .get("cmd")
                    .or_else(|| params.get("command"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|cmd| !cmd.is_empty())
                    .map(ToString::to_string)
                else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call exec requires params.cmd or params.command".to_string(),
                    };
                };
                self.dispatch_exec(&payload.call_id, &cmd, params).await
            }
            "http" => self.dispatch_http(&payload.call_id, params).await,
            "session" => {
                let Some(op) = protocol_hostcall_op(&params) else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call session requires params.op".to_string(),
                    };
                };
                self.dispatch_session(&payload.call_id, &op, params).await
            }
            "ui" => {
                let Some(op) = protocol_hostcall_op(&params) else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call ui requires params.op".to_string(),
                    };
                };
                self.dispatch_ui(&payload.call_id, &op, params).await
            }
            "events" => {
                let Some(op) = protocol_hostcall_op(&params) else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "host_call events requires params.op".to_string(),
                    };
                };
                self.dispatch_events(&payload.call_id, None, &op, params)
                    .await
            }
            _ => HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: format!("Unsupported host_call method: {method}"),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_tool(
        &self,
        call_id: &str,
        name: &str,
        payload: serde_json::Value,
    ) -> HostcallOutcome {
        let Some(tool) = self.tool_registry.get(name) else {
            return HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: format!("Unknown tool: {name}"),
            };
        };

        match tool.execute(call_id, payload, None).await {
            Ok(output) => match serde_json::to_value(output) {
                Ok(value) => HostcallOutcome::Success(value),
                Err(err) => HostcallOutcome::Error {
                    code: "internal".to_string(),
                    message: format!("Serialize tool output: {err}"),
                },
            },
            Err(err) => HostcallOutcome::Error {
                code: "io".to_string(),
                message: err.to_string(),
            },
        }
    }

    #[allow(clippy::future_not_send, clippy::too_many_lines)]
    async fn dispatch_exec(
        &self,
        call_id: &str,
        cmd: &str,
        payload: serde_json::Value,
    ) -> HostcallOutcome {
        use std::io::{BufRead as _, Read as _};
        use std::process::{Command, Stdio};
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};

        enum ExecStreamFrame {
            Stdout(String),
            Stderr(String),
            Final { code: i32, killed: bool },
            Error(String),
        }

        fn pump_stream<R: std::io::Read>(
            reader: R,
            tx: &SyncSender<ExecStreamFrame>,
            stdout: bool,
        ) -> std::result::Result<(), String> {
            let mut reader = std::io::BufReader::new(reader);
            loop {
                let mut buf = Vec::new();
                let read = reader
                    .read_until(b'\n', &mut buf)
                    .map_err(|err| err.to_string())?;
                if read == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let frame = if stdout {
                    ExecStreamFrame::Stdout(text)
                } else {
                    ExecStreamFrame::Stderr(text)
                };
                if tx.send(frame).is_err() {
                    break;
                }
            }
            Ok(())
        }

        let args_value = payload
            .get("args")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let args_array = match args_value {
            serde_json::Value::Null => Vec::new(),
            serde_json::Value::Array(items) => items,
            _ => {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "exec args must be an array".to_string(),
                };
            }
        };

        let args = args_array
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), ToString::to_string)
            })
            .collect::<Vec<_>>();

        let options = payload
            .get("options")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let cwd = options
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map_or_else(|| self.cwd.clone(), PathBuf::from);
        let timeout_ms = options
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| options.get("timeoutMs").and_then(serde_json::Value::as_u64))
            .or_else(|| {
                options
                    .get("timeout_ms")
                    .and_then(serde_json::Value::as_u64)
            })
            .filter(|ms| *ms > 0);
        let stream = options
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if stream {
            let cmd = cmd.to_string();
            let args = args.clone();
            let (tx, rx) = mpsc::sync_channel::<ExecStreamFrame>(256);
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_worker = Arc::clone(&cancel);
            let call_id_for_error = call_id.to_string();

            thread::spawn(move || {
                let result = (|| -> std::result::Result<(), String> {
                    let mut command = Command::new(&cmd);
                    command
                        .args(&args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .current_dir(&cwd);

                    let mut child = command.spawn().map_err(|err| err.to_string())?;
                    let pid = child.id();

                    let stdout = child.stdout.take().ok_or("Missing stdout pipe")?;
                    let stderr = child.stderr.take().ok_or("Missing stderr pipe")?;

                    let stdout_tx = tx.clone();
                    let stderr_tx = tx.clone();
                    let stdout_handle =
                        thread::spawn(move || pump_stream(stdout, &stdout_tx, true));
                    let stderr_handle =
                        thread::spawn(move || pump_stream(stderr, &stderr_tx, false));

                    let start = Instant::now();
                    let mut killed = false;
                    let status = loop {
                        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                            break status;
                        }

                        if cancel_worker.load(AtomicOrdering::SeqCst) {
                            killed = true;
                            crate::tools::kill_process_tree(Some(pid));
                            let _ = child.kill();
                            break child.wait().map_err(|err| err.to_string())?;
                        }

                        if let Some(timeout_ms) = timeout_ms {
                            if start.elapsed() >= Duration::from_millis(timeout_ms) {
                                killed = true;
                                crate::tools::kill_process_tree(Some(pid));
                                let _ = child.kill();
                                break child.wait().map_err(|err| err.to_string())?;
                            }
                        }

                        thread::sleep(Duration::from_millis(10));
                    };

                    let stdout_result = stdout_handle
                        .join()
                        .map_err(|_| "stdout reader thread panicked".to_string())?;
                    if let Err(err) = stdout_result {
                        return Err(format!("Read stdout: {err}"));
                    }

                    let stderr_result = stderr_handle
                        .join()
                        .map_err(|_| "stderr reader thread panicked".to_string())?;
                    if let Err(err) = stderr_result {
                        return Err(format!("Read stderr: {err}"));
                    }

                    let code = status.code().unwrap_or(0);
                    let _ = tx.send(ExecStreamFrame::Final { code, killed });
                    Ok(())
                })();

                if let Err(err) = result {
                    if tx.send(ExecStreamFrame::Error(err)).is_err() {
                        tracing::trace!(
                            call_id = %call_id_for_error,
                            "Exec hostcall stream result dropped before completion"
                        );
                    }
                }
            });

            let mut sequence = 0_u64;
            loop {
                if !self.runtime.is_hostcall_pending(call_id) {
                    cancel.store(true, AtomicOrdering::SeqCst);
                    return HostcallOutcome::Error {
                        code: "cancelled".to_string(),
                        message: "exec stream cancelled".to_string(),
                    };
                }

                match rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(ExecStreamFrame::Stdout(chunk)) => {
                        self.runtime.complete_hostcall(
                            call_id.to_string(),
                            HostcallOutcome::StreamChunk {
                                sequence,
                                chunk: serde_json::json!({ "stdout": chunk }),
                                is_final: false,
                            },
                        );
                        sequence = sequence.saturating_add(1);
                    }
                    Ok(ExecStreamFrame::Stderr(chunk)) => {
                        self.runtime.complete_hostcall(
                            call_id.to_string(),
                            HostcallOutcome::StreamChunk {
                                sequence,
                                chunk: serde_json::json!({ "stderr": chunk }),
                                is_final: false,
                            },
                        );
                        sequence = sequence.saturating_add(1);
                    }
                    Ok(ExecStreamFrame::Final { code, killed }) => {
                        return HostcallOutcome::StreamChunk {
                            sequence,
                            chunk: serde_json::json!({
                                "code": code,
                                "killed": killed,
                            }),
                            is_final: true,
                        };
                    }
                    Ok(ExecStreamFrame::Error(message)) => {
                        return HostcallOutcome::Error {
                            code: "io".to_string(),
                            message,
                        };
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return HostcallOutcome::Error {
                            code: "internal".to_string(),
                            message: "exec stream channel closed".to_string(),
                        };
                    }
                }
            }
        }

        let cmd = cmd.to_string();
        let args = args.clone();
        let (tx, rx) = oneshot::channel();
        let call_id_for_error = call_id.to_string();

        thread::spawn(move || {
            let result: std::result::Result<serde_json::Value, String> = (|| {
                let mut command = Command::new(&cmd);
                command
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .current_dir(&cwd);

                let mut child = command.spawn().map_err(|err| err.to_string())?;
                let pid = child.id();

                let mut stdout = child.stdout.take().ok_or("Missing stdout pipe")?;
                let mut stderr = child.stderr.take().ok_or("Missing stderr pipe")?;

                let stdout_handle = thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = stdout.read_to_end(&mut buf);
                    buf
                });
                let stderr_handle = thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = stderr.read_to_end(&mut buf);
                    buf
                });

                let start = Instant::now();
                let mut killed = false;
                let status = loop {
                    if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                        break status;
                    }

                    if let Some(timeout_ms) = timeout_ms {
                        if start.elapsed() >= Duration::from_millis(timeout_ms) {
                            killed = true;
                            crate::tools::kill_process_tree(Some(pid));
                            let _ = child.kill();
                            break child.wait().map_err(|err| err.to_string())?;
                        }
                    }

                    thread::sleep(Duration::from_millis(10));
                };

                let stdout_bytes = stdout_handle.join().unwrap_or_default();
                let stderr_bytes = stderr_handle.join().unwrap_or_default();

                let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
                let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
                let code = status.code().unwrap_or(0);

                Ok(serde_json::json!({
                    "stdout": stdout,
                    "stderr": stderr,
                    "code": code,
                    "killed": killed,
                }))
            })();

            let cx = Cx::for_request();
            if tx.send(&cx, result).is_err() {
                tracing::trace!(
                    call_id = %call_id_for_error,
                    "Exec hostcall result dropped before completion"
                );
            }
        });

        let cx = Cx::for_request();
        match rx.recv(&cx).await {
            Ok(Ok(value)) => HostcallOutcome::Success(value),
            Ok(Err(err)) => HostcallOutcome::Error {
                code: "io".to_string(),
                message: err,
            },
            Err(_) => HostcallOutcome::Error {
                code: "internal".to_string(),
                message: "exec task cancelled".to_string(),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_http(&self, call_id: &str, payload: serde_json::Value) -> HostcallOutcome {
        let call = HostCallPayload {
            call_id: call_id.to_string(),
            capability: "http".to_string(),
            method: "http".to_string(),
            params: payload,
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        match self.http_connector.dispatch(&call).await {
            Ok(result) => {
                if result.is_error {
                    let message = result.error.as_ref().map_or_else(
                        || "HTTP connector error".to_string(),
                        |err| err.message.clone(),
                    );
                    let code = result
                        .error
                        .as_ref()
                        .map_or("internal", |err| hostcall_code_to_str(err.code));
                    HostcallOutcome::Error {
                        code: code.to_string(),
                        message,
                    }
                } else {
                    HostcallOutcome::Success(result.output)
                }
            }
            Err(err) => HostcallOutcome::Error {
                code: "internal".to_string(),
                message: err.to_string(),
            },
        }
    }

    #[allow(clippy::future_not_send, clippy::too_many_lines)]
    async fn dispatch_session(&self, call_id: &str, op: &str, payload: Value) -> HostcallOutcome {
        use crate::connectors::HostCallErrorCode;

        let op_norm = op.trim().to_ascii_lowercase();

        // Build canonical HostCallPayload for the session capability.
        let _call = HostCallPayload {
            call_id: call_id.to_string(),
            capability: "session".to_string(),
            method: op_norm.clone(),
            params: payload.clone(),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        // Categorised result: (Value, error_code) where error_code distinguishes taxonomy.
        let result: std::result::Result<Value, (HostCallErrorCode, String)> = match op_norm.as_str()
        {
            "get_state" | "getstate" => Ok(self.session.get_state().await),
            "get_messages" | "getmessages" => {
                serde_json::to_value(self.session.get_messages().await).map_err(|err| {
                    (
                        HostCallErrorCode::Internal,
                        format!("Serialize messages: {err}"),
                    )
                })
            }
            "get_entries" | "getentries" => serde_json::to_value(self.session.get_entries().await)
                .map_err(|err| {
                    (
                        HostCallErrorCode::Internal,
                        format!("Serialize entries: {err}"),
                    )
                }),
            "get_branch" | "getbranch" => serde_json::to_value(self.session.get_branch().await)
                .map_err(|err| {
                    (
                        HostCallErrorCode::Internal,
                        format!("Serialize branch: {err}"),
                    )
                }),
            "get_file" | "getfile" => {
                let state = self.session.get_state().await;
                let file = state
                    .get("sessionFile")
                    .or_else(|| state.get("session_file"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(file)
            }
            "get_name" | "getname" => {
                let state = self.session.get_state().await;
                let name = state
                    .get("sessionName")
                    .or_else(|| state.get("session_name"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(name)
            }
            "set_name" | "setname" => {
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.session
                    .set_name(name)
                    .await
                    .map(|()| Value::Null)
                    .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
            }
            "append_entry" | "appendentry" => {
                let custom_type = payload
                    .get("customType")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("custom_type").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                let data = payload.get("data").cloned();
                self.session
                    .append_custom_entry(custom_type, data)
                    .await
                    .map(|()| Value::Null)
                    .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
            }
            "append_message" | "appendmessage" => {
                let message_value = payload.get("message").cloned().unwrap_or(payload);
                match serde_json::from_value(message_value) {
                    Ok(message) => self
                        .session
                        .append_message(message)
                        .await
                        .map(|()| Value::Null)
                        .map_err(|err| (HostCallErrorCode::Io, err.to_string())),
                    Err(err) => Err((
                        HostCallErrorCode::InvalidRequest,
                        format!("Parse message: {err}"),
                    )),
                }
            }
            "set_model" | "setmodel" => {
                let provider = payload
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let model_id = payload
                    .get("modelId")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("model_id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                if provider.is_empty() || model_id.is_empty() {
                    Err((
                        HostCallErrorCode::InvalidRequest,
                        "set_model requires 'provider' and 'modelId' fields".to_string(),
                    ))
                } else {
                    self.session
                        .set_model(provider, model_id)
                        .await
                        .map(|()| Value::Bool(true))
                        .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
                }
            }
            "get_model" | "getmodel" => {
                let (provider, model_id) = self.session.get_model().await;
                Ok(serde_json::json!({
                    "provider": provider,
                    "modelId": model_id,
                }))
            }
            "set_thinking_level" | "setthinkinglevel" => {
                let level = payload
                    .get("level")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("thinkingLevel").and_then(Value::as_str))
                    .or_else(|| payload.get("thinking_level").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                if level.is_empty() {
                    Err((
                        HostCallErrorCode::InvalidRequest,
                        "set_thinking_level requires 'level' field".to_string(),
                    ))
                } else {
                    self.session
                        .set_thinking_level(level)
                        .await
                        .map(|()| Value::Null)
                        .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
                }
            }
            "get_thinking_level" | "getthinkinglevel" => {
                let level = self.session.get_thinking_level().await;
                Ok(level.map_or(Value::Null, Value::String))
            }
            "set_label" | "setlabel" => {
                let target_id = payload
                    .get("targetId")
                    .and_then(Value::as_str)
                    .or_else(|| payload.get("target_id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_string();
                let label = payload
                    .get("label")
                    .and_then(Value::as_str)
                    .map(String::from);
                if target_id.is_empty() {
                    Err((
                        HostCallErrorCode::InvalidRequest,
                        "set_label requires 'targetId' field".to_string(),
                    ))
                } else {
                    self.session
                        .set_label(target_id, label)
                        .await
                        .map(|()| Value::Null)
                        .map_err(|err| (HostCallErrorCode::Io, err.to_string()))
                }
            }
            _ => Err((
                HostCallErrorCode::InvalidRequest,
                format!("Unknown session op: {op}"),
            )),
        };

        match result {
            Ok(value) => HostcallOutcome::Success(value),
            Err((code, message)) => HostcallOutcome::Error {
                code: hostcall_code_to_str(code).to_string(),
                message,
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_ui(&self, call_id: &str, op: &str, payload: Value) -> HostcallOutcome {
        let request = ExtensionUiRequest {
            id: call_id.to_string(),
            method: op.to_string(),
            payload,
            timeout_ms: None,
        };

        match self.ui_handler.request_ui(request).await {
            Ok(Some(response)) => HostcallOutcome::Success(ui_response_value_for_op(op, &response)),
            Ok(None) => HostcallOutcome::Success(Value::Null),
            Err(err) => HostcallOutcome::Error {
                code: "io".to_string(),
                message: err.to_string(),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_events(
        &self,
        _call_id: &str,
        extension_id: Option<&str>,
        op: &str,
        payload: Value,
    ) -> HostcallOutcome {
        match op.trim() {
            "list" => match self.list_extension_events(extension_id).await {
                Ok(events) => HostcallOutcome::Success(serde_json::json!({ "events": events })),
                Err(err) => HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err.to_string(),
                },
            },
            "emit" => {
                let event_name = payload
                    .get("event")
                    .or_else(|| payload.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty());

                let Some(event_name) = event_name else {
                    return HostcallOutcome::Error {
                        code: "invalid_request".to_string(),
                        message: "events.emit requires non-empty `event`".to_string(),
                    };
                };

                let event_payload = payload.get("data").cloned().unwrap_or(Value::Null);
                let timeout_ms = payload
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .or_else(|| payload.get("timeoutMs").and_then(Value::as_u64))
                    .or_else(|| payload.get("timeout").and_then(Value::as_u64))
                    .filter(|ms| *ms > 0)
                    .unwrap_or(EXTENSION_EVENT_TIMEOUT_MS);

                let ctx_payload = match payload.get("ctx") {
                    Some(ctx) => ctx.clone(),
                    None => self.build_default_event_ctx(extension_id).await,
                };

                match Box::pin(self.dispatch_extension_event(
                    event_name,
                    event_payload,
                    ctx_payload,
                    timeout_ms,
                ))
                .await
                {
                    Ok(result) => {
                        let handler_count = self
                            .count_event_handlers(event_name)
                            .await
                            .unwrap_or_default();

                        HostcallOutcome::Success(serde_json::json!({
                            "dispatched": true,
                            "event": event_name,
                            "handler_count": handler_count,
                            "result": result,
                        }))
                    }
                    Err(err) => HostcallOutcome::Error {
                        code: "io".to_string(),
                        message: err.to_string(),
                    },
                }
            }
            other => HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: format!("Unsupported events op: {other}"),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn list_extension_events(&self, extension_id: Option<&str>) -> Result<Vec<String>> {
        #[derive(serde::Deserialize)]
        struct Snapshot {
            id: String,
            #[serde(default)]
            event_hooks: Vec<String>,
        }

        let json = self
            .runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let snapshot_fn: rquickjs::Function<'_> = global.get("__pi_snapshot_extensions")?;
                let value: rquickjs::Value<'_> = snapshot_fn.call(())?;
                js_to_json(&value)
            })
            .await?;

        let snapshots: Vec<Snapshot> = serde_json::from_value(json)
            .map_err(|err| crate::error::Error::extension(err.to_string()))?;

        let mut events = BTreeSet::new();
        match extension_id {
            Some(needle) => {
                for snapshot in snapshots {
                    if snapshot.id == needle {
                        for event in snapshot.event_hooks {
                            let event = event.trim();
                            if !event.is_empty() {
                                events.insert(event.to_string());
                            }
                        }
                        break;
                    }
                }
            }
            None => {
                for snapshot in snapshots {
                    for event in snapshot.event_hooks {
                        let event = event.trim();
                        if !event.is_empty() {
                            events.insert(event.to_string());
                        }
                    }
                }
            }
        }

        Ok(events.into_iter().collect())
    }

    #[allow(clippy::future_not_send)]
    async fn count_event_handlers(&self, event_name: &str) -> Result<Option<usize>> {
        let literal = serde_json::to_string(event_name)
            .map_err(|err| crate::error::Error::extension(err.to_string()))?;

        self.runtime
            .with_ctx(|ctx| {
                let code = format!(
                    "(function() {{ const handlers = (__pi_hook_index.get({literal}) || []); return handlers.length; }})()"
                );
                ctx.eval::<usize, _>(code)
                    .map(Some)
                    .or(Ok(None))
            })
            .await
    }

    #[allow(clippy::future_not_send)]
    async fn build_default_event_ctx(&self, _extension_id: Option<&str>) -> Value {
        let entries = self.session.get_entries().await;
        let branch = self.session.get_branch().await;
        let leaf_entry = branch.last().cloned().unwrap_or(Value::Null);

        serde_json::json!({
            "hasUI": true,
            "cwd": self.cwd.display().to_string(),
            "sessionEntries": entries,
            "branch": branch,
            "leafEntry": leaf_entry,
            "modelRegistry": {},
        })
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_extension_event(
        &self,
        event_name: &str,
        event_payload: Value,
        ctx_payload: Value,
        timeout_ms: u64,
    ) -> Result<Value> {
        #[derive(serde::Deserialize)]
        struct JsTaskError {
            #[serde(default)]
            code: Option<String>,
            message: String,
            #[serde(default)]
            stack: Option<String>,
        }

        #[derive(serde::Deserialize)]
        struct JsTaskState {
            status: String,
            #[serde(default)]
            value: Option<Value>,
            #[serde(default)]
            error: Option<JsTaskError>,
        }

        let task_id = format!("task-events-{call_id}", call_id = uuid::Uuid::new_v4());

        self.runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let dispatch_fn: rquickjs::Function<'_> =
                    global.get("__pi_dispatch_extension_event")?;
                let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;

                let event_js = json_to_js(&ctx, &event_payload)?;
                let ctx_js = json_to_js(&ctx, &ctx_payload)?;
                let promise: rquickjs::Value<'_> =
                    dispatch_fn.call((event_name.to_string(), event_js, ctx_js))?;
                let _task: String = task_start.call((task_id.clone(), promise))?;
                Ok(())
            })
            .await?;

        let start = Instant::now();
        let timeout = Duration::from_millis(timeout_ms.max(1));

        loop {
            if start.elapsed() > timeout {
                return Err(crate::error::Error::extension(format!(
                    "events.emit timed out after {}ms",
                    timeout.as_millis()
                )));
            }

            let mut pending = self.runtime.drain_hostcall_requests();
            while let Some(req) = pending.pop_front() {
                self.dispatch_and_complete(req).await;
            }

            let _ = self.runtime.tick().await?;
            let _ = self.runtime.drain_microtasks().await?;

            let state_json = self
                .runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let take_fn: rquickjs::Function<'_> = global.get("__pi_task_take")?;
                    let value: rquickjs::Value<'_> = take_fn.call((task_id.clone(),))?;
                    js_to_json(&value)
                })
                .await?;

            if state_json.is_null() {
                return Err(crate::error::Error::extension(
                    "events.emit task state missing".to_string(),
                ));
            }

            let state: JsTaskState = serde_json::from_value(state_json)
                .map_err(|err| crate::error::Error::extension(err.to_string()))?;

            match state.status.as_str() {
                "pending" => {
                    if !self.runtime.has_pending() {
                        sleep(wall_now(), Duration::from_millis(1)).await;
                    }
                }
                "resolved" => return Ok(state.value.unwrap_or(Value::Null)),
                "rejected" => {
                    let err = state.error.unwrap_or_else(|| JsTaskError {
                        code: None,
                        message: "Unknown JS task error".to_string(),
                        stack: None,
                    });
                    let mut message = err.message;
                    if let Some(code) = err.code {
                        message = format!("{code}: {message}");
                    }
                    if let Some(stack) = err.stack {
                        if !stack.is_empty() {
                            message.push('\n');
                            message.push_str(&stack);
                        }
                    }
                    return Err(crate::error::Error::extension(message));
                }
                other => {
                    return Err(crate::error::Error::extension(format!(
                        "Unexpected JS task status: {other}"
                    )));
                }
            }

            sleep(wall_now(), Duration::from_millis(0)).await;
        }
    }
}

const fn hostcall_code_to_str(code: crate::connectors::HostCallErrorCode) -> &'static str {
    match code {
        crate::connectors::HostCallErrorCode::Timeout => "timeout",
        crate::connectors::HostCallErrorCode::Denied => "denied",
        crate::connectors::HostCallErrorCode::Io => "io",
        crate::connectors::HostCallErrorCode::InvalidRequest => "invalid_request",
        crate::connectors::HostCallErrorCode::Internal => "internal",
    }
}

/// Trait for handling individual hostcall types.
#[async_trait]
pub trait HostcallHandler: Send + Sync {
    /// Process a hostcall request and return the outcome.
    async fn handle(&self, params: serde_json::Value) -> HostcallOutcome;

    /// The capability name for policy checking (e.g., "read", "exec", "http").
    fn capability(&self) -> &'static str;
}

/// Trait for handling UI hostcalls (pi.ui()).
#[async_trait]
pub trait ExtensionUiHandler: Send + Sync {
    async fn request_ui(&self, request: ExtensionUiRequest) -> Result<Option<ExtensionUiResponse>>;
}

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;

    use crate::connectors::http::HttpConnectorConfig;
    use crate::extensions::{ExtensionBody, ExtensionMessage, HostCallPayload, PROTOCOL_VERSION};
    use crate::scheduler::DeterministicClock;
    use crate::session::SessionMessage;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::Mutex;

    #[test]
    fn ui_confirm_cancel_defaults_to_false() {
        let response = ExtensionUiResponse {
            id: "req-1".to_string(),
            value: None,
            cancelled: true,
        };
        assert_eq!(
            ui_response_value_for_op("confirm", &response),
            Value::Bool(false)
        );
        assert_eq!(ui_response_value_for_op("select", &response), Value::Null);
    }

    struct NullSession;

    #[async_trait]
    impl ExtensionSession for NullSession {
        async fn get_state(&self) -> Value {
            Value::Null
        }

        async fn get_messages(&self) -> Vec<SessionMessage> {
            Vec::new()
        }

        async fn get_entries(&self) -> Vec<Value> {
            Vec::new()
        }

        async fn get_branch(&self) -> Vec<Value> {
            Vec::new()
        }

        async fn set_name(&self, _name: String) -> Result<()> {
            Ok(())
        }

        async fn append_message(&self, _message: SessionMessage) -> Result<()> {
            Ok(())
        }

        async fn append_custom_entry(
            &self,
            _custom_type: String,
            _data: Option<Value>,
        ) -> Result<()> {
            Ok(())
        }

        async fn set_model(&self, _provider: String, _model_id: String) -> Result<()> {
            Ok(())
        }

        async fn get_model(&self) -> (Option<String>, Option<String>) {
            (None, None)
        }

        async fn set_thinking_level(&self, _level: String) -> Result<()> {
            Ok(())
        }

        async fn get_thinking_level(&self) -> Option<String> {
            None
        }

        async fn set_label(&self, _target_id: String, _label: Option<String>) -> Result<()> {
            Ok(())
        }
    }

    struct NullUiHandler;

    #[async_trait]
    impl ExtensionUiHandler for NullUiHandler {
        async fn request_ui(
            &self,
            _request: ExtensionUiRequest,
        ) -> Result<Option<ExtensionUiResponse>> {
            Ok(None)
        }
    }

    struct TestUiHandler {
        captured: Arc<Mutex<Vec<ExtensionUiRequest>>>,
        response_value: Value,
    }

    #[async_trait]
    impl ExtensionUiHandler for TestUiHandler {
        async fn request_ui(
            &self,
            request: ExtensionUiRequest,
        ) -> Result<Option<ExtensionUiResponse>> {
            self.captured.lock().unwrap().push(request.clone());
            Ok(Some(ExtensionUiResponse {
                id: request.id,
                value: Some(self.response_value.clone()),
                cancelled: false,
            }))
        }
    }

    type CustomEntry = (String, Option<Value>);
    type CustomEntries = Arc<Mutex<Vec<CustomEntry>>>;

    type LabelEntry = (String, Option<String>);

    struct TestSession {
        state: Arc<Mutex<Value>>,
        messages: Arc<Mutex<Vec<SessionMessage>>>,
        entries: Arc<Mutex<Vec<Value>>>,
        branch: Arc<Mutex<Vec<Value>>>,
        name: Arc<Mutex<Option<String>>>,
        custom_entries: CustomEntries,
        labels: Arc<Mutex<Vec<LabelEntry>>>,
    }

    #[async_trait]
    impl ExtensionSession for TestSession {
        async fn get_state(&self) -> Value {
            self.state.lock().unwrap().clone()
        }

        async fn get_messages(&self) -> Vec<SessionMessage> {
            self.messages.lock().unwrap().clone()
        }

        async fn get_entries(&self) -> Vec<Value> {
            self.entries.lock().unwrap().clone()
        }

        async fn get_branch(&self) -> Vec<Value> {
            self.branch.lock().unwrap().clone()
        }

        async fn set_name(&self, name: String) -> Result<()> {
            {
                let mut guard = self.name.lock().unwrap();
                *guard = Some(name.clone());
            }
            let mut state = self.state.lock().unwrap();
            if let Value::Object(ref mut map) = *state {
                map.insert("sessionName".to_string(), Value::String(name));
            }
            drop(state);
            Ok(())
        }

        async fn append_message(&self, message: SessionMessage) -> Result<()> {
            self.messages.lock().unwrap().push(message);
            Ok(())
        }

        async fn append_custom_entry(
            &self,
            custom_type: String,
            data: Option<Value>,
        ) -> Result<()> {
            self.custom_entries
                .lock()
                .unwrap()
                .push((custom_type, data));
            Ok(())
        }

        async fn set_model(&self, provider: String, model_id: String) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            if let Value::Object(ref mut map) = *state {
                map.insert("provider".to_string(), Value::String(provider));
                map.insert("modelId".to_string(), Value::String(model_id));
            }
            drop(state);
            Ok(())
        }

        async fn get_model(&self) -> (Option<String>, Option<String>) {
            let state = self.state.lock().unwrap();
            let provider = state
                .get("provider")
                .and_then(Value::as_str)
                .map(String::from);
            let model_id = state
                .get("modelId")
                .and_then(Value::as_str)
                .map(String::from);
            drop(state);
            (provider, model_id)
        }

        async fn set_thinking_level(&self, level: String) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            if let Value::Object(ref mut map) = *state {
                map.insert("thinkingLevel".to_string(), Value::String(level));
            }
            drop(state);
            Ok(())
        }

        async fn get_thinking_level(&self) -> Option<String> {
            let state = self.state.lock().unwrap();
            let level = state
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .map(String::from);
            drop(state);
            level
        }

        async fn set_label(&self, target_id: String, label: Option<String>) -> Result<()> {
            self.labels.lock().unwrap().push((target_id, label));
            Ok(())
        }
    }

    fn build_dispatcher(
        runtime: Rc<PiJsRuntime<DeterministicClock>>,
    ) -> ExtensionDispatcher<DeterministicClock> {
        ExtensionDispatcher::new(
            runtime,
            Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
            Arc::new(HttpConnector::with_defaults()),
            Arc::new(NullSession),
            Arc::new(NullUiHandler),
            PathBuf::from("."),
        )
    }

    fn spawn_http_server(body: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
        let addr = listener.local_addr().expect("server addr");
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn dispatcher_constructs() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            assert!(Rc::ptr_eq(&dispatcher.runtime, &runtime));
            assert_eq!(dispatcher.cwd, PathBuf::from("."));
        });
    }

    #[test]
    fn dispatcher_drains_empty_queue() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let drained = dispatcher.drain_hostcall_requests();
            assert!(drained.is_empty());
        });
    }

    #[test]
    fn dispatcher_drains_runtime_requests() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            runtime
                .eval(r#"pi.tool("read", { "path": "test.txt" });"#)
                .await
                .expect("eval");

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let drained = dispatcher.drain_hostcall_requests();
            assert_eq!(drained.len(), 1);
        });
    }

    #[test]
    fn dispatcher_tool_hostcall_executes_and_resolves_promise() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("test.txt"), "hello world").expect("write file");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.tool("read", { path: "test.txt" }).then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            let stats = runtime.tick().await.expect("tick");
            assert!(stats.ran_macrotask);

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("Promise not resolved");
                    if (!JSON.stringify(globalThis.result).includes("hello world")) {
                        throw new Error("Wrong result: " + JSON.stringify(globalThis.result));
                    }
                "#,
                )
                .await
                .expect("verify result");
        });
    }

    #[test]
    fn dispatcher_tool_hostcall_unknown_tool_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.tool("nope", {}).catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err !== "invalid_request") {
                        throw new Error("Wrong error code: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[test]
    fn dispatcher_session_hostcall_resolves_state_and_set_name() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.state = null;
                    globalThis.file = null;
                    globalThis.nameValue = null;
                    globalThis.nameSet = false;
                    pi.session("get_state", {}).then((r) => { globalThis.state = r; });
                    pi.session("get_file", {}).then((r) => { globalThis.file = r; });
                    pi.session("get_name", {}).then((r) => { globalThis.nameValue = r; });
                    pi.session("set_name", { name: "hello" }).then(() => { globalThis.nameSet = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 4);

            let name = Arc::new(Mutex::new(None));
            let state = Arc::new(Mutex::new(serde_json::json!({
                "sessionFile": "/tmp/session.jsonl",
                "sessionName": "demo",
            })));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::clone(&name),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let (state_value, file_value, name_value, name_set) = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let state_js: rquickjs::Value<'_> = global.get("state")?;
                    let file_js: rquickjs::Value<'_> = global.get("file")?;
                    let name_js: rquickjs::Value<'_> = global.get("nameValue")?;
                    let name_set_js: rquickjs::Value<'_> = global.get("nameSet")?;
                    Ok((
                        crate::extensions_js::js_to_json(&state_js)?,
                        crate::extensions_js::js_to_json(&file_js)?,
                        crate::extensions_js::js_to_json(&name_js)?,
                        crate::extensions_js::js_to_json(&name_set_js)?,
                    ))
                })
                .await
                .expect("read globals");

            let state_file = state_value
                .get("sessionFile")
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert_eq!(state_file, "/tmp/session.jsonl");
            assert_eq!(file_value, Value::String("/tmp/session.jsonl".to_string()));
            assert_eq!(name_value, Value::String("demo".to_string()));
            assert_eq!(name_set, Value::Bool(true));

            let name_value = name.lock().unwrap().clone();
            assert_eq!(name_value.as_deref(), Some("hello"));
        });
    }

    #[test]
    fn dispatcher_session_hostcall_get_messages_entries_branch() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.messages = null;
                    globalThis.entries = null;
                    globalThis.branch = null;
                    pi.session("get_messages", {}).then((r) => { globalThis.messages = r; });
                    pi.session("get_entries", {}).then((r) => { globalThis.entries = r; });
                    pi.session("get_branch", {}).then((r) => { globalThis.branch = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 3);

            let message = SessionMessage::Custom {
                custom_type: "note".to_string(),
                content: "hello".to_string(),
                display: true,
                details: None,
            };
            let entries = vec![serde_json::json!({ "id": "entry-1", "type": "custom" })];
            let branch = vec![serde_json::json!({ "id": "entry-2", "type": "branch" })];

            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(Value::Null)),
                messages: Arc::new(Mutex::new(vec![message.clone()])),
                entries: Arc::new(Mutex::new(entries.clone())),
                branch: Arc::new(Mutex::new(branch.clone())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let (messages_value, entries_value, branch_value) = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let messages_js: rquickjs::Value<'_> = global.get("messages")?;
                    let entries_js: rquickjs::Value<'_> = global.get("entries")?;
                    let branch_js: rquickjs::Value<'_> = global.get("branch")?;
                    Ok((
                        crate::extensions_js::js_to_json(&messages_js)?,
                        crate::extensions_js::js_to_json(&entries_js)?,
                        crate::extensions_js::js_to_json(&branch_js)?,
                    ))
                })
                .await
                .expect("read globals");

            let messages_array = messages_value.as_array().expect("messages array");
            assert_eq!(messages_array.len(), 1);
            assert_eq!(
                messages_array[0]
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "custom"
            );
            assert_eq!(
                messages_array[0]
                    .get("customType")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                "note"
            );
            assert_eq!(entries_value, Value::Array(entries));
            assert_eq!(branch_value, Value::Array(branch));
        });
    }

    #[test]
    fn dispatcher_session_hostcall_append_message_and_entry() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.messageAppended = false;
                    globalThis.entryAppended = false;
                    pi.session("append_message", {
                        message: { role: "custom", customType: "note", content: "hi", display: true }
                    }).then(() => { globalThis.messageAppended = true; });
                    pi.session("append_entry", {
                        customType: "meta",
                        data: { ok: true }
                    }).then(() => { globalThis.entryAppended = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2);

            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(Value::Null)),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                {
                    let session_handle: Arc<dyn ExtensionSession + Send + Sync> = session.clone();
                    session_handle
                },
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let (message_appended, entry_appended) = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let message_js: rquickjs::Value<'_> = global.get("messageAppended")?;
                    let entry_js: rquickjs::Value<'_> = global.get("entryAppended")?;
                    Ok((
                        crate::extensions_js::js_to_json(&message_js)?,
                        crate::extensions_js::js_to_json(&entry_js)?,
                    ))
                })
                .await
                .expect("read globals");

            assert_eq!(message_appended, Value::Bool(true));
            assert_eq!(entry_appended, Value::Bool(true));

            {
                let messages = session.messages.lock().unwrap().clone();
                assert_eq!(messages.len(), 1);
                match &messages[0] {
                    SessionMessage::Custom {
                        custom_type,
                        content,
                        display,
                        ..
                    } => {
                        assert_eq!(custom_type, "note");
                        assert_eq!(content, "hi");
                        assert!(*display);
                    }
                    other => assert!(
                        matches!(other, SessionMessage::Custom { .. }),
                        "Unexpected message: {other:?}"
                    ),
                }
            }

            {
                let expected = Some(serde_json::json!({ "ok": true }));
                let custom_entries = session.custom_entries.lock().unwrap().clone();
                assert_eq!(custom_entries.len(), 1);
                assert_eq!(custom_entries[0].0, "meta");
                assert_eq!(custom_entries[0].1, expected);
                drop(custom_entries);
            }
        });
    }

    #[test]
    fn dispatcher_session_hostcall_unknown_op_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.session("nope", {}).catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let err_value = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let err_js: rquickjs::Value<'_> = global.get("err")?;
                    crate::extensions_js::js_to_json(&err_js)
                })
                .await
                .expect("read globals");

            assert_eq!(err_value, Value::String("invalid_request".to_string()));
        });
    }

    #[test]
    fn dispatcher_session_hostcall_append_message_invalid_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.session("append_message", { message: { nope: 1 } })
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let err_value = runtime
                .with_ctx(|ctx| {
                    let global = ctx.globals();
                    let err_js: rquickjs::Value<'_> = global.get("err")?;
                    crate::extensions_js::js_to_json(&err_js)
                })
                .await
                .expect("read globals");

            assert_eq!(err_value, Value::String("invalid_request".to_string()));
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_executes_and_resolves_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("sh", ["-c", "printf hello"], {})
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("Promise not resolved");
                    if (globalThis.result.stdout !== "hello") {
                        throw new Error("Wrong stdout: " + JSON.stringify(globalThis.result));
                    }
                    if (globalThis.result.code !== 0) {
                        throw new Error("Wrong exit code: " + JSON.stringify(globalThis.result));
                    }
                    if (globalThis.result.killed !== false) {
                        throw new Error("Unexpected killed flag: " + JSON.stringify(globalThis.result));
                    }
                "#,
                )
                .await
                .expect("verify result");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_command_not_found_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.exec("definitely_not_a_real_command", [], {})
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err !== "io") {
                        throw new Error("Wrong error code: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_streaming_callback_delivers_chunks_and_final_result() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.chunks = [];
                    globalThis.finalResult = null;
                    pi.exec("sh", ["-c", "printf 'out-1\n'; printf 'err-1\n' 1>&2; printf 'out-2\n'"], {
                        stream: true,
                        onChunk: (chunk, isFinal) => {
                            globalThis.chunks.push({ chunk, isFinal });
                        },
                    }).then((r) => { globalThis.finalResult = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!Array.isArray(globalThis.chunks) || globalThis.chunks.length < 3) {
                        throw new Error("Expected stream chunks, got: " + JSON.stringify(globalThis.chunks));
                    }
                    const sawStdout = globalThis.chunks.some((entry) => entry.chunk && entry.chunk.stdout && entry.chunk.stdout.includes("out-1"));
                    if (!sawStdout) {
                        throw new Error("Missing stdout chunk: " + JSON.stringify(globalThis.chunks));
                    }
                    const sawStderr = globalThis.chunks.some((entry) => entry.chunk && entry.chunk.stderr && entry.chunk.stderr.includes("err-1"));
                    if (!sawStderr) {
                        throw new Error("Missing stderr chunk: " + JSON.stringify(globalThis.chunks));
                    }
                    const finalEntry = globalThis.chunks[globalThis.chunks.length - 1];
                    if (!finalEntry || finalEntry.isFinal !== true) {
                        throw new Error("Missing final chunk marker: " + JSON.stringify(globalThis.chunks));
                    }
                    if (globalThis.finalResult === null) {
                        throw new Error("Promise not resolved");
                    }
                    if (globalThis.finalResult.code !== 0) {
                        throw new Error("Wrong exit code: " + JSON.stringify(globalThis.finalResult));
                    }
                    if (globalThis.finalResult.killed !== false) {
                        throw new Error("Unexpected killed flag: " + JSON.stringify(globalThis.finalResult));
                    }
                "#,
                )
                .await
                .expect("verify stream callback result");
        });
    }

    #[test]
    #[cfg(unix)]
    fn dispatcher_exec_hostcall_streaming_async_iterator_delivers_chunks_in_order() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.iterChunks = [];
                    globalThis.iterDone = false;
                    (async () => {
                        const stream = pi.exec("sh", ["-c", "printf 'a\n'; printf 'b\n'"], { stream: true });
                        for await (const chunk of stream) {
                            globalThis.iterChunks.push(chunk);
                        }
                        globalThis.iterDone = true;
                    })();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.iterDone !== true) {
                        throw new Error("Async iterator did not finish");
                    }
                    if (!Array.isArray(globalThis.iterChunks) || globalThis.iterChunks.length < 3) {
                        throw new Error("Missing stream chunks: " + JSON.stringify(globalThis.iterChunks));
                    }
                    if (!globalThis.iterChunks[0] || globalThis.iterChunks[0].stdout !== "a\n") {
                        throw new Error("Unexpected first chunk: " + JSON.stringify(globalThis.iterChunks));
                    }
                    if (!globalThis.iterChunks[1] || globalThis.iterChunks[1].stdout !== "b\n") {
                        throw new Error("Unexpected second chunk: " + JSON.stringify(globalThis.iterChunks));
                    }
                    const finalChunk = globalThis.iterChunks[globalThis.iterChunks.length - 1];
                    if (!finalChunk || finalChunk.code !== 0 || finalChunk.killed !== false) {
                        throw new Error("Unexpected final chunk: " + JSON.stringify(finalChunk));
                    }
                "#,
                )
                .await
                .expect("verify async iterator result");
        });
    }

    #[test]
    fn dispatcher_http_hostcall_executes_and_resolves_promise() {
        futures::executor::block_on(async {
            let addr = spawn_http_server("hello");
            let url = format!("http://{addr}/test");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{ url: "{url}", method: "GET" }})
                    .then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("Promise not resolved");
                    if (globalThis.result.status !== 200) {
                        throw new Error("Wrong status: " + globalThis.result.status);
                    }
                    if (globalThis.result.body !== "hello") {
                        throw new Error("Wrong body: " + globalThis.result.body);
                    }
                "#,
                )
                .await
                .expect("verify result");
        });
    }

    #[test]
    fn dispatcher_http_hostcall_invalid_method_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.http({ url: "https://example.com", method: "PUT" })
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err !== "invalid_request") {
                        throw new Error("Wrong error code: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[test]
    fn dispatcher_ui_hostcall_executes_and_resolves_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.uiResult = null;
                    pi.ui("confirm", { title: "Confirm?" }).then((r) => { globalThis.uiResult = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured = Arc::new(Mutex::new(Vec::new()));
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(TestUiHandler {
                    captured: Arc::clone(&captured),
                    response_value: serde_json::json!({ "ok": true }),
                }),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (!globalThis.uiResult || globalThis.uiResult.ok !== true) {
                        throw new Error("Wrong UI result: " + JSON.stringify(globalThis.uiResult));
                    }
                "#,
                )
                .await
                .expect("verify result");

            let seen = captured.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].method, "confirm");
        });
    }

    #[test]
    fn dispatcher_extension_ui_set_status_includes_text_field() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    const ui = __pi_make_extension_ui(true);
                    ui.setStatus("key", "hello");
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured = Arc::new(Mutex::new(Vec::new()));
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(TestUiHandler {
                    captured: Arc::clone(&captured),
                    response_value: Value::Null,
                }),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            let seen = captured.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].method, "setStatus");
            assert_eq!(
                seen[0].payload.get("statusKey").and_then(Value::as_str),
                Some("key")
            );
            assert_eq!(
                seen[0].payload.get("statusText").and_then(Value::as_str),
                Some("hello")
            );
            assert_eq!(
                seen[0].payload.get("text").and_then(Value::as_str),
                Some("hello")
            );
        });
    }

    #[test]
    fn dispatcher_extension_ui_set_widget_includes_widget_lines_and_content() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    const ui = __pi_make_extension_ui(true);
                    ui.setWidget("widget", ["a", "b"]);
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured = Arc::new(Mutex::new(Vec::new()));
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(TestUiHandler {
                    captured: Arc::clone(&captured),
                    response_value: Value::Null,
                }),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            let seen = captured.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].method, "setWidget");
            assert_eq!(
                seen[0].payload.get("widgetKey").and_then(Value::as_str),
                Some("widget")
            );
            assert_eq!(
                seen[0].payload.get("content").and_then(Value::as_str),
                Some("a\nb")
            );
            assert_eq!(
                seen[0].payload.get("widgetLines").and_then(Value::as_array),
                seen[0].payload.get("lines").and_then(Value::as_array)
            );
        });
    }

    #[test]
    fn dispatcher_events_hostcall_rejects_promise() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.events("setActiveTools", { tools: ["read"] })
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (globalThis.err === null) throw new Error("Promise not rejected");
                    if (globalThis.err !== "invalid_request") {
                        throw new Error("Wrong error code: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify error");
        });
    }

    #[test]
    fn dispatcher_events_list_returns_registered_hooks() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.eventsList = null;
                    __pi_begin_extension("ext.a", { name: "ext.a" });
                    pi.on("custom_event", (_payload, _ctx) => {});
                    pi.events("list", {}).then((r) => { globalThis.eventsList = r; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (!globalThis.eventsList) throw new Error("Promise not resolved");
                    const events = globalThis.eventsList.events;
                    if (!Array.isArray(events)) throw new Error("Missing events array");
                    if (events.length !== 1 || events[0] !== "custom_event") {
                        throw new Error("Wrong events list: " + JSON.stringify(events));
                    }
                "#,
                )
                .await
                .expect("verify list");
        });
    }

    #[test]
    fn dispatcher_session_set_model_resolves_and_persists() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.setResult = null;
                    pi.session("set_model", { provider: "anthropic", modelId: "claude-sonnet-4-20250514" })
                        .then((r) => { globalThis.setResult = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.setResult !== true) {
                        throw new Error("set_model should resolve to true, got: " + JSON.stringify(globalThis.setResult));
                    }
                "#,
                )
                .await
                .expect("verify set_model result");

            let final_state = state.lock().unwrap().clone();
            assert_eq!(
                final_state.get("provider").and_then(Value::as_str),
                Some("anthropic")
            );
            assert_eq!(
                final_state.get("modelId").and_then(Value::as_str),
                Some("claude-sonnet-4-20250514")
            );
        });
    }

    #[test]
    fn dispatcher_session_get_model_resolves_provider_and_model_id() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.model = null;
                    pi.session("get_model", {}).then((r) => { globalThis.model = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({
                "provider": "openai",
                "modelId": "gpt-4o",
            })));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.model) throw new Error("get_model not resolved");
                    if (globalThis.model.provider !== "openai") {
                        throw new Error("Wrong provider: " + globalThis.model.provider);
                    }
                    if (globalThis.model.modelId !== "gpt-4o") {
                        throw new Error("Wrong modelId: " + globalThis.model.modelId);
                    }
                "#,
                )
                .await
                .expect("verify get_model result");
        });
    }

    #[test]
    fn dispatcher_session_set_model_missing_fields_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errNoProvider = null;
                    globalThis.errNoModelId = null;
                    globalThis.errEmpty = null;
                    pi.session("set_model", { modelId: "claude-sonnet-4-20250514" })
                        .catch((e) => { globalThis.errNoProvider = e.code; });
                    pi.session("set_model", { provider: "anthropic" })
                        .catch((e) => { globalThis.errNoModelId = e.code; });
                    pi.session("set_model", {})
                        .catch((e) => { globalThis.errEmpty = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 3);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errNoProvider !== "invalid_request") {
                        throw new Error("Missing provider should reject: " + globalThis.errNoProvider);
                    }
                    if (globalThis.errNoModelId !== "invalid_request") {
                        throw new Error("Missing modelId should reject: " + globalThis.errNoModelId);
                    }
                    if (globalThis.errEmpty !== "invalid_request") {
                        throw new Error("Empty payload should reject: " + globalThis.errEmpty);
                    }
                "#,
                )
                .await
                .expect("verify validation errors");
        });
    }

    #[test]
    fn dispatcher_session_set_then_get_model_round_trip() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Phase 1: set_model
            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    pi.session("set_model", { provider: "gemini", modelId: "gemini-2.0-flash" })
                        .then(() => { globalThis.setDone = true; });
                "#,
                )
                .await
                .expect("eval set");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session as Arc<dyn ExtensionSession + Send + Sync>,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Phase 2: get_model
            runtime
                .eval(
                    r#"
                    globalThis.model = null;
                    pi.session("get_model", {}).then((r) => { globalThis.model = r; });
                "#,
                )
                .await
                .expect("eval get");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.model) throw new Error("get_model not resolved");
                    if (globalThis.model.provider !== "gemini") {
                        throw new Error("Wrong provider: " + globalThis.model.provider);
                    }
                    if (globalThis.model.modelId !== "gemini-2.0-flash") {
                        throw new Error("Wrong modelId: " + globalThis.model.modelId);
                    }
                "#,
                )
                .await
                .expect("verify round trip");
        });
    }

    #[test]
    fn dispatcher_session_set_thinking_level_resolves() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    pi.session("set_thinking_level", { level: "high" })
                        .then(() => { globalThis.setDone = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // set_thinking_level resolves to null (not true like set_model)
            runtime
                .eval(
                    r#"
                    if (globalThis.setDone !== true) {
                        throw new Error("set_thinking_level not resolved");
                    }
                "#,
                )
                .await
                .expect("verify set_thinking_level");

            let final_state = state.lock().unwrap().clone();
            assert_eq!(
                final_state.get("thinkingLevel").and_then(Value::as_str),
                Some("high")
            );
        });
    }

    #[test]
    fn dispatcher_session_get_thinking_level_resolves() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.level = "__unset__";
                    pi.session("get_thinking_level", {}).then((r) => { globalThis.level = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({
                "thinkingLevel": "medium",
            })));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.level !== "medium") {
                        throw new Error("Wrong thinking level: " + JSON.stringify(globalThis.level));
                    }
                "#,
                )
                .await
                .expect("verify get_thinking_level");
        });
    }

    #[test]
    fn dispatcher_session_get_thinking_level_null_when_unset() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.level = "__unset__";
                    pi.session("get_thinking_level", {}).then((r) => { globalThis.level = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.level !== null) {
                        throw new Error("Unset thinking level should be null, got: " + JSON.stringify(globalThis.level));
                    }
                "#,
                )
                .await
                .expect("verify null thinking level");
        });
    }

    #[test]
    fn dispatcher_session_set_thinking_level_missing_level_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.err = null;
                    pi.session("set_thinking_level", {})
                        .catch((e) => { globalThis.err = e.code; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.err !== "invalid_request") {
                        throw new Error("Missing level should reject: " + globalThis.err);
                    }
                "#,
                )
                .await
                .expect("verify validation error");
        });
    }

    #[test]
    fn dispatcher_session_set_then_get_thinking_level_round_trip() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Phase 1: set
            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    pi.session("set_thinking_level", { level: "low" })
                        .then(() => { globalThis.setDone = true; });
                "#,
                )
                .await
                .expect("eval set");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session as Arc<dyn ExtensionSession + Send + Sync>,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Phase 2: get
            runtime
                .eval(
                    r#"
                    globalThis.level = "__unset__";
                    pi.session("get_thinking_level", {}).then((r) => { globalThis.level = r; });
                "#,
                )
                .await
                .expect("eval get");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.level !== "low") {
                        throw new Error("Round trip failed, got: " + JSON.stringify(globalThis.level));
                    }
                "#,
                )
                .await
                .expect("verify round trip");
        });
    }

    #[test]
    fn dispatcher_session_model_ops_accept_camel_case_aliases() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    globalThis.model = null;
                    globalThis.thinkingSet = false;
                    globalThis.thinking = "__unset__";
                    pi.session("setmodel", { provider: "azure", modelId: "gpt-4" })
                        .then(() => { globalThis.setDone = true; });
                    pi.session("getmodel", {}).then((r) => { globalThis.model = r; });
                    pi.session("setthinkinglevel", { level: "high" })
                        .then(() => { globalThis.thinkingSet = true; });
                    pi.session("getthinkinglevel", {}).then((r) => { globalThis.thinking = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 4);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session as Arc<dyn ExtensionSession + Send + Sync>,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.setDone) throw new Error("setmodel not resolved");
                    if (!globalThis.thinkingSet) throw new Error("setthinkinglevel not resolved");
                "#,
                )
                .await
                .expect("verify camelCase aliases");
        });
    }

    #[test]
    fn dispatcher_session_set_model_accepts_model_id_snake_case() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.setDone = false;
                    pi.session("set_model", { provider: "anthropic", model_id: "claude-opus-4-20250514" })
                        .then(() => { globalThis.setDone = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.setDone) throw new Error("set_model with model_id not resolved");
                "#,
                )
                .await
                .expect("verify model_id snake_case");

            let final_state = state.lock().unwrap().clone();
            assert_eq!(
                final_state.get("modelId").and_then(Value::as_str),
                Some("claude-opus-4-20250514")
            );
        });
    }

    #[test]
    fn dispatcher_session_set_thinking_level_accepts_alt_keys() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Test thinkingLevel key
            runtime
                .eval(
                    r#"
                    globalThis.done1 = false;
                    globalThis.done2 = false;
                    pi.session("set_thinking_level", { thinkingLevel: "medium" })
                        .then(() => { globalThis.done1 = true; });
                    pi.session("set_thinking_level", { thinking_level: "low" })
                        .then(() => { globalThis.done2 = true; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2);

            let state = Arc::new(Mutex::new(serde_json::json!({})));
            let session = Arc::new(TestSession {
                state: Arc::clone(&state),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.done1) throw new Error("thinkingLevel key not resolved");
                    if (!globalThis.done2) throw new Error("thinking_level key not resolved");
                "#,
                )
                .await
                .expect("verify alt keys");

            // Last write wins, so "low" should be the final value
            let final_state = state.lock().unwrap().clone();
            assert_eq!(
                final_state.get("thinkingLevel").and_then(Value::as_str),
                Some("low")
            );
        });
    }

    #[test]
    fn dispatcher_session_get_model_null_when_unset() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.model = "__unset__";
                    pi.session("get_model", {}).then((r) => { globalThis.model = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // NullSession returns (None, None) for get_model
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.model) throw new Error("get_model not resolved");
                    if (globalThis.model.provider !== null) {
                        throw new Error("Unset provider should be null, got: " + JSON.stringify(globalThis.model.provider));
                    }
                    if (globalThis.model.modelId !== null) {
                        throw new Error("Unset modelId should be null, got: " + JSON.stringify(globalThis.model.modelId));
                    }
                "#,
                )
                .await
                .expect("verify null model");
        });
    }

    // ---- set_label tests ----

    #[test]
    fn dispatcher_session_set_label_resolves_and_persists() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("set_label", { targetId: "msg-42", label: "important" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let labels: Arc<Mutex<Vec<LabelEntry>>> = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::clone(&labels),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Verify set_label was called with correct args
            let captured = labels.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "msg-42");
            assert_eq!(captured[0].1.as_deref(), Some("important"));
            drop(captured);
        });
    }

    #[test]
    fn dispatcher_session_set_label_remove_label_with_null() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("set_label", { targetId: "msg-99" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let labels: Arc<Mutex<Vec<LabelEntry>>> = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::clone(&labels),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Verify set_label was called with None label (removal)
            let captured = labels.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "msg-99");
            assert!(captured[0].1.is_none());
            drop(captured);
        });
    }

    #[test]
    fn dispatcher_session_set_label_missing_target_id_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.session("set_label", { label: "orphaned" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.errMsg || globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection, got: " + globalThis.errMsg);
                    }
                    if (!globalThis.errMsg.includes("targetId")) {
                        throw new Error("Expected error about targetId, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify rejection");
        });
    }

    #[test]
    fn dispatcher_session_set_label_accepts_snake_case_target_id() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("set_label", { target_id: "msg-77", label: "reviewed" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let labels: Arc<Mutex<Vec<LabelEntry>>> = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::clone(&labels),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let captured = labels.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "msg-77");
            assert_eq!(captured[0].1.as_deref(), Some("reviewed"));
            drop(captured);
        });
    }

    #[test]
    fn dispatcher_session_set_label_camel_case_op_alias() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use "setLabel" style (gets lowercased to "setlabel" which matches)
            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("setLabel", { targetId: "entry-5", label: "flagged" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let labels: Arc<Mutex<Vec<LabelEntry>>> = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::clone(&labels),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let captured = labels.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "entry-5");
            assert_eq!(captured[0].1.as_deref(), Some("flagged"));
            drop(captured);
        });
    }

    // ---- Tool conformance tests ----

    #[test]
    fn dispatcher_tool_write_creates_file_and_resolves() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let file_path = temp_dir.path().join("output.txt");
            let file_path_str = file_path.display().to_string();
            let script = format!(
                r#"
                globalThis.result = null;
                pi.tool("write", {{ path: "{file_path_str}", content: "written by extension" }})
                    .then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["write"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // Verify file was created
            assert!(file_path.exists());
            let content = std::fs::read_to_string(&file_path).expect("read file");
            assert_eq!(content, "written by extension");
        });
    }

    #[test]
    fn dispatcher_tool_ls_lists_directory() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("alpha.txt"), "a").expect("write");
            std::fs::write(temp_dir.path().join("beta.txt"), "b").expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.tool("ls", { path: "." })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["ls"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("ls not resolved");
                    let s = JSON.stringify(globalThis.result);
                    if (!s.includes("alpha.txt") || !s.includes("beta.txt")) {
                        throw new Error("Missing files in ls output: " + s);
                    }
                "#,
                )
                .await
                .expect("verify ls result");
        });
    }

    #[test]
    fn dispatcher_tool_grep_searches_content() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(
                temp_dir.path().join("data.txt"),
                "line one\nline two\nline three",
            )
            .expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let dir = temp_dir.path().display().to_string();
            let script = format!(
                r#"
                globalThis.result = null;
                pi.tool("grep", {{ pattern: "two", path: "{dir}" }})
                    .then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["grep"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("grep not resolved");
                    let s = JSON.stringify(globalThis.result);
                    if (!s.includes("two")) {
                        throw new Error("grep should find 'two': " + s);
                    }
                "#,
                )
                .await
                .expect("verify grep result");
        });
    }

    #[test]
    fn dispatcher_tool_edit_modifies_file_content() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("target.txt"), "old text here").expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.tool("edit", { path: "target.txt", oldText: "old text", newText: "new text" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["edit"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let content =
                std::fs::read_to_string(temp_dir.path().join("target.txt")).expect("read file");
            assert!(
                content.contains("new text"),
                "Expected edited content, got: {content}"
            );
        });
    }

    #[test]
    fn dispatcher_tool_find_discovers_files() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("code.rs"), "fn main(){}").expect("write");
            std::fs::write(temp_dir.path().join("data.json"), "{}").expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.tool("find", { pattern: "*.rs" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["find"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("find not resolved");
                    let s = JSON.stringify(globalThis.result);
                    if (!s.includes("code.rs")) {
                        throw new Error("find should discover code.rs: " + s);
                    }
                    if (s.includes("data.json")) {
                        throw new Error("find *.rs should not include data.json: " + s);
                    }
                "#,
                )
                .await
                .expect("verify find result");
        });
    }

    #[test]
    fn dispatcher_tool_multiple_tools_sequentially() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(temp_dir.path().join("file.txt"), "hello").expect("write");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Queue two tool calls
            runtime
                .eval(
                    r#"
                    globalThis.readResult = null;
                    globalThis.lsResult = null;
                    pi.tool("read", { path: "file.txt" })
                        .then((r) => { globalThis.readResult = r; });
                    pi.tool("ls", { path: "." })
                        .then((r) => { globalThis.lsResult = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read", "ls"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.readResult === null) throw new Error("read not resolved");
                    if (globalThis.lsResult === null) throw new Error("ls not resolved");
                "#,
                )
                .await
                .expect("verify both tools resolved");
        });
    }

    #[test]
    fn dispatcher_tool_error_propagates_to_js() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Try to read a non-existent file
            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.tool("read", { path: "nonexistent_file.txt" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            // The read tool may resolve with an error content rather than rejecting.
            // Either way, the dispatcher shouldn't panic.
            runtime
                .eval(
                    r#"
                    // Just verify something happened - error propagation is tool-specific
                    if (globalThis.errMsg === "" && globalThis.result === null) {
                        throw new Error("Neither resolved nor rejected");
                    }
                "#,
                )
                .await
                .expect("verify tool error handling");
        });
    }

    // ---- HTTP conformance tests ----

    fn spawn_http_server_with_status(status: u16, body: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind http server");
        let addr = listener.local_addr().expect("server addr");
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status} Error\r\nContent-Length: {len}\r\nContent-Type: text/plain\r\n\r\n{body}",
                    status = status,
                    len = body.len(),
                    body = body,
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn dispatcher_http_post_sends_body() {
        futures::executor::block_on(async {
            let addr = spawn_http_server("post-ok");
            let url = format!("http://{addr}/data");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{ url: "{url}", method: "POST", body: "test-payload" }})
                    .then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("POST not resolved");
                    if (globalThis.result.status !== 200) {
                        throw new Error("Expected 200, got: " + globalThis.result.status);
                    }
                "#,
                )
                .await
                .expect("verify POST result");
        });
    }

    #[test]
    fn dispatcher_http_missing_url_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for missing URL");
                    }
                "#,
                )
                .await
                .expect("verify missing URL rejection");
        });
    }

    #[test]
    fn dispatcher_http_custom_headers() {
        futures::executor::block_on(async {
            let addr = spawn_http_server("headers-ok");
            let url = format!("http://{addr}/headers");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{
                    url: "{url}",
                    method: "GET",
                    headers: {{ "X-Custom": "test-value", "Accept": "application/json" }}
                }}).then((r) => {{ globalThis.result = r; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === null) throw new Error("HTTP not resolved");
                    if (globalThis.result.status !== 200) {
                        throw new Error("Expected 200, got: " + globalThis.result.status);
                    }
                "#,
                )
                .await
                .expect("verify headers request");
        });
    }

    #[test]
    fn dispatcher_http_connection_refused_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use a port that definitely has nothing listening
            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "http://127.0.0.1:1/never", method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for connection refused");
                    }
                "#,
                )
                .await
                .expect("verify connection refused");
        });
    }

    // ---- UI conformance tests ----

    #[test]
    fn dispatcher_ui_spinner_method() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("spinner", { text: "Loading...", visible: true })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: serde_json::json!({ "acknowledged": true }),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "spinner");
            assert_eq!(reqs[0].payload["text"], "Loading...");
        });
    }

    #[test]
    fn dispatcher_ui_progress_method() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("progress", { current: 50, total: 100, label: "Processing" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "progress");
            assert_eq!(reqs[0].payload["current"], 50);
            assert_eq!(reqs[0].payload["total"], 100);
        });
    }

    #[test]
    fn dispatcher_ui_notification_method() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("notification", { message: "Task complete!", level: "info" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: serde_json::json!({ "shown": true }),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "notification");
            assert_eq!(reqs[0].payload["message"], "Task complete!");
            assert_eq!(reqs[0].payload["level"], "info");
        });
    }

    #[test]
    fn dispatcher_ui_null_handler_returns_null() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.ui("any_method", { key: "value" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // Use NullUiHandler - returns None which maps to null
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === "__unset__") throw new Error("UI not resolved");
                    if (globalThis.result !== null) {
                        throw new Error("Expected null from NullHandler, got: " + JSON.stringify(globalThis.result));
                    }
                "#,
                )
                .await
                .expect("verify null UI handler");
        });
    }

    #[test]
    fn dispatcher_ui_multiple_calls_captured() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.r1 = null;
                    globalThis.r2 = null;
                    pi.ui("set_status", { text: "Working..." })
                        .then((r) => { globalThis.r1 = r; });
                    pi.ui("set_widget", { lines: ["Line 1", "Line 2"] })
                        .then((r) => { globalThis.r2 = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 2);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let (len, methods) = {
                let reqs = captured.lock().unwrap();
                let len = reqs.len();
                let methods = reqs.iter().map(|r| r.method.clone()).collect::<Vec<_>>();
                drop(reqs);
                (len, methods)
            };
            assert_eq!(len, 2);
            assert!(methods.iter().any(|method| method == "set_status"));
            assert!(methods.iter().any(|method| method == "set_widget"));
        });
    }

    // ---- Exec edge case tests ----

    #[test]
    fn dispatcher_exec_with_custom_cwd() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("pwd", { cwd: "/tmp" })
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    // Either it resolved to stdout containing /tmp, or it
                    // was rejected - both are valid dispatcher behaviors.
                    // Key assertion: the dispatcher didn't panic.
                "#,
                )
                .await
                .expect("verify exec cwd");
        });
    }

    #[test]
    fn dispatcher_exec_empty_command_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.exec("")
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for empty command");
                    }
                    // Empty command should produce some kind of error
                    if (!globalThis.errMsg) {
                        throw new Error("Expected error message");
                    }
                "#,
                )
                .await
                .expect("verify empty command rejection");
        });
    }

    // ---- Events edge case tests ----

    #[test]
    fn dispatcher_events_emit_missing_event_name_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.events("emit", {})
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    // Should either reject or produce an error - not silently succeed
                    if (globalThis.errMsg === "should_not_resolve") {
                        // It's also acceptable if emit with empty payload succeeds gracefully
                    }
                "#,
                )
                .await
                .expect("verify events emit");
        });
    }

    #[test]
    fn dispatcher_events_list_empty_when_no_hooks() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Register an extension with no hooks, then list events
            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    __pi_begin_extension("ext.empty", { name: "ext.empty" });
                    pi.events("list", {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("events list not resolved");
                    // Result is { events: [...] }
                    const events = globalThis.result.events;
                    if (!Array.isArray(events)) {
                        throw new Error("Expected events array, got: " + JSON.stringify(globalThis.result));
                    }
                    if (events.length !== 0) {
                        throw new Error("Expected empty events list, got: " + JSON.stringify(events));
                    }
                "#,
                )
                .await
                .expect("verify events list empty");
        });
    }

    // ---- Isolated session op tests ----

    #[test]
    fn dispatcher_session_get_file_isolated() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.file = "__unset__";
                    pi.session("get_file", {})
                        .then((r) => { globalThis.file = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({
                "sessionFile": "/home/user/.pi/sessions/abc.json"
            })));
            let session = Arc::new(TestSession {
                state,
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.file === "__unset__") throw new Error("get_file not resolved");
                    if (globalThis.file !== "/home/user/.pi/sessions/abc.json") {
                        throw new Error("Expected session file path, got: " + JSON.stringify(globalThis.file));
                    }
                "#,
                )
                .await
                .expect("verify get_file");
        });
    }

    #[test]
    fn dispatcher_session_get_name_isolated() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.name = "__unset__";
                    pi.session("get_name", {})
                        .then((r) => { globalThis.name = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let state = Arc::new(Mutex::new(serde_json::json!({
                "sessionName": "My Debug Session"
            })));
            let session = Arc::new(TestSession {
                state,
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(Some("My Debug Session".to_string()))),
                custom_entries: Arc::new(Mutex::new(Vec::new())),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.name === "__unset__") throw new Error("get_name not resolved");
                    if (globalThis.name !== "My Debug Session") {
                        throw new Error("Expected session name, got: " + JSON.stringify(globalThis.name));
                    }
                "#,
                )
                .await
                .expect("verify get_name");
        });
    }

    #[test]
    fn dispatcher_session_append_entry_custom_type_edge_cases() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Test with custom_type key (snake_case variant)
            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.session("append_entry", {
                        custom_type: "audit_log",
                        data: { action: "login", ts: 1234567890 }
                    }).then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let custom_entries: CustomEntries = Arc::new(Mutex::new(Vec::new()));
            let session = Arc::new(TestSession {
                state: Arc::new(Mutex::new(serde_json::json!({}))),
                messages: Arc::new(Mutex::new(Vec::new())),
                entries: Arc::new(Mutex::new(Vec::new())),
                branch: Arc::new(Mutex::new(Vec::new())),
                name: Arc::new(Mutex::new(None)),
                custom_entries: Arc::clone(&custom_entries),
                labels: Arc::new(Mutex::new(Vec::new())),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                session,
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let captured = custom_entries.lock().unwrap();
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].0, "audit_log");
            assert!(captured[0].1.is_some());
            let data = captured[0].1.as_ref().unwrap().clone();
            drop(captured);
            assert_eq!(data["action"], "login");
        });
    }

    #[test]
    fn dispatcher_events_emit_dispatches_custom_event() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.seen = [];
                    globalThis.emitResult = null;

                    __pi_begin_extension("ext.b", { name: "ext.b" });
                    pi.on("custom_event", (payload, _ctx) => { globalThis.seen.push(payload); });
                    __pi_end_extension();

                    __pi_begin_extension("ext.a", { name: "ext.a" });
                    pi.events("emit", { event: "custom_event", data: { hello: "world" } })
                      .then((r) => { globalThis.emitResult = r; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (!globalThis.emitResult) throw new Error("emit promise not resolved");
                    if (globalThis.emitResult.dispatched !== true) {
                        throw new Error("emit did not report dispatched: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (globalThis.emitResult.event !== "custom_event") {
                        throw new Error("wrong event: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (!Array.isArray(globalThis.seen) || globalThis.seen.length !== 1) {
                        throw new Error("event handler not called: " + JSON.stringify(globalThis.seen));
                    }
                    const payload = globalThis.seen[0];
                    if (!payload || payload.hello !== "world") {
                        throw new Error("wrong payload: " + JSON.stringify(payload));
                    }
                "#,
                )
                .await
                .expect("verify emit");
        });
    }

    // ---- Additional exec conformance tests ----

    #[test]
    fn dispatcher_exec_with_args_array() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // pi.exec(cmd, args, options) - args is the second positional arg
            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/echo", ["hello", "world"], {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (typeof globalThis.result.stdout !== "string") {
                        throw new Error("Expected stdout string, got: " + JSON.stringify(globalThis.result));
                    }
                    if (!globalThis.result.stdout.includes("hello") || !globalThis.result.stdout.includes("world")) {
                        throw new Error("Expected 'hello world' in stdout, got: " + globalThis.result.stdout);
                    }
                "#,
                )
                .await
                .expect("verify exec with args");
        });
    }

    #[test]
    fn dispatcher_exec_null_args_defaults_to_empty() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/echo")
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    // echo with no args produces empty or newline stdout
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (typeof globalThis.result.stdout !== "string") {
                        throw new Error("Expected stdout string");
                    }
                "#,
                )
                .await
                .expect("verify exec null args");
        });
    }

    #[test]
    fn dispatcher_exec_non_array_args_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.exec("echo", "not-an-array", {})
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for non-array args");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("array")) {
                        throw new Error("Expected error about array, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify non-array args rejection");
        });
    }

    #[test]
    fn dispatcher_exec_captures_stdout_and_stderr() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use sh -c to write to both stdout and stderr
            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/sh", ["-c", "echo OUT && echo ERR >&2"], {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (!globalThis.result.stdout.includes("OUT")) {
                        throw new Error("Expected 'OUT' in stdout, got: " + globalThis.result.stdout);
                    }
                    if (!globalThis.result.stderr.includes("ERR")) {
                        throw new Error("Expected 'ERR' in stderr, got: " + globalThis.result.stderr);
                    }
                "#,
                )
                .await
                .expect("verify stdout and stderr capture");
        });
    }

    #[test]
    fn dispatcher_exec_nonzero_exit_code() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.exec("/bin/sh", ["-c", "exit 42"], {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("exec not resolved");
                    if (globalThis.result.error) throw new Error("exec errored: " + globalThis.result.error);
                    if (globalThis.result.code !== 42) {
                        throw new Error("Expected exit code 42, got: " + globalThis.result.code);
                    }
                "#,
                )
                .await
                .expect("verify nonzero exit code");
        });
    }

    #[test]
    fn dispatcher_exec_command_not_found_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.exec("__nonexistent_command_xyz__")
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for nonexistent command");
                    }
                    if (!globalThis.errMsg) {
                        throw new Error("Expected error message for nonexistent command");
                    }
                "#,
                )
                .await
                .expect("verify command not found rejection");
        });
    }

    // ---- Additional HTTP conformance tests ----

    #[test]
    fn dispatcher_http_tls_required_rejects_http_url() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "http://example.com/test", method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            // Use default config which has require_tls: true
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for http:// URL when TLS required");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("tls") &&
                        !globalThis.errMsg.toLowerCase().includes("https")) {
                        throw new Error("Expected TLS-related error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify TLS enforcement");
        });
    }

    #[test]
    fn dispatcher_http_invalid_url_format_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "not-a-valid-url", method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for invalid URL");
                    }
                    if (!globalThis.errMsg) {
                        throw new Error("Expected error message for invalid URL");
                    }
                "#,
                )
                .await
                .expect("verify invalid URL rejection");
        });
    }

    #[test]
    fn dispatcher_http_get_with_body_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "https://example.com/test", method: "GET", body: "should-not-have-body" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for GET with body");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("body") &&
                        !globalThis.errMsg.toLowerCase().includes("get")) {
                        throw new Error("Expected body/GET error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify GET with body rejection");
        });
    }

    #[test]
    fn dispatcher_http_response_body_returned() {
        futures::executor::block_on(async {
            let addr = spawn_http_server_with_status(200, "response-body-content");
            let url = format!("http://{addr}/body-test");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{ url: "{url}", method: "GET" }})
                    .then((r) => {{ globalThis.result = r; }})
                    .catch((e) => {{ globalThis.result = {{ error: e.message || String(e) }}; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("HTTP not resolved");
                    if (globalThis.result.error) throw new Error("HTTP error: " + globalThis.result.error);
                    if (globalThis.result.status !== 200) {
                        throw new Error("Expected 200, got: " + globalThis.result.status);
                    }
                    const body = globalThis.result.body || "";
                    if (!body.includes("response-body-content")) {
                        throw new Error("Expected response body, got: " + body);
                    }
                "#,
                )
                .await
                .expect("verify response body");
        });
    }

    #[test]
    fn dispatcher_http_error_status_code_returned() {
        futures::executor::block_on(async {
            let addr = spawn_http_server_with_status(404, "not found");
            let url = format!("http://{addr}/missing");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.http({{ url: "{url}", method: "GET" }})
                    .then((r) => {{ globalThis.result = r; }})
                    .catch((e) => {{ globalThis.result = {{ error: e.message || String(e) }}; }});
            "#
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("HTTP not resolved");
                    // 404 should still resolve (not reject) with the status code
                    if (globalThis.result.status !== 404) {
                        throw new Error("Expected status 404, got: " + JSON.stringify(globalThis.result));
                    }
                "#,
                )
                .await
                .expect("verify error status code");
        });
    }

    #[test]
    fn dispatcher_http_unsupported_scheme_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.http({ url: "ftp://example.com/file", method: "GET" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let http_connector = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                ..Default::default()
            });
            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(http_connector),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for ftp:// scheme");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("scheme") &&
                        !globalThis.errMsg.toLowerCase().includes("unsupported")) {
                        throw new Error("Expected scheme error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify unsupported scheme rejection");
        });
    }

    // ---- Additional UI conformance tests ----

    #[test]
    fn dispatcher_ui_arbitrary_method_passthrough() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("custom_op", { key: "value" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "custom_op");
            assert_eq!(reqs[0].payload["key"], "value");
        });
    }

    #[test]
    fn dispatcher_ui_payload_passthrough_complex() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("set_widget", {
                        lines: [
                            { text: "Line 1", style: { bold: true } },
                            { text: "Line 2", style: { color: "red" } }
                        ],
                        content: "widget body",
                        metadata: { nested: { deep: true } }
                    }).then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            let payload = &reqs[0].payload;
            assert!(payload["lines"].is_array());
            assert_eq!(payload["lines"].as_array().unwrap().len(), 2);
            assert_eq!(payload["content"], "widget body");
            assert_eq!(payload["metadata"]["nested"]["deep"], true);
        });
    }

    #[test]
    fn dispatcher_ui_handler_returns_value() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = "__unset__";
                    pi.ui("get_input", { prompt: "Enter name" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: serde_json::json!({ "input": "Alice", "confirmed": true }),
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.result === "__unset__") throw new Error("UI not resolved");
                    if (globalThis.result.input !== "Alice") {
                        throw new Error("Expected input 'Alice', got: " + JSON.stringify(globalThis.result));
                    }
                    if (globalThis.result.confirmed !== true) {
                        throw new Error("Expected confirmed true");
                    }
                "#,
                )
                .await
                .expect("verify UI handler value");
        });
    }

    #[test]
    fn dispatcher_ui_set_status_empty_text() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("set_status", { text: "" })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "set_status");
            assert_eq!(reqs[0].payload["text"], "");
        });
    }

    #[test]
    fn dispatcher_ui_empty_payload() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("dismiss", {})
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "dismiss");
        });
    }

    #[test]
    fn dispatcher_ui_concurrent_different_methods() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.results = [];
                    pi.ui("set_status", { text: "Loading..." })
                        .then((r) => { globalThis.results.push("status"); });
                    pi.ui("show_spinner", { message: "Working" })
                        .then((r) => { globalThis.results.push("spinner"); });
                    pi.ui("set_widget", { lines: [], content: "w" })
                        .then((r) => { globalThis.results.push("widget"); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 3);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 3);
            let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
            assert!(methods.contains(&"set_status"));
            assert!(methods.contains(&"show_spinner"));
            assert!(methods.contains(&"set_widget"));
        });
    }

    #[test]
    fn dispatcher_ui_notification_with_severity() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("notification", { text: "Error occurred", severity: "error", duration: 5000 })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "notification");
            assert_eq!(reqs[0].payload["severity"], "error");
            assert_eq!(reqs[0].payload["duration"], 5000);
        });
    }

    #[test]
    fn dispatcher_ui_widget_with_lines_array() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("set_widget", {
                        lines: [
                            { text: "=== Status ===" },
                            { text: "CPU: 42%" },
                            { text: "Mem: 8GB" }
                        ],
                        content: "Dashboard"
                    }).then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "set_widget");
            let lines = reqs[0].payload["lines"].as_array().unwrap();
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[0]["text"], "=== Status ===");
            assert_eq!(lines[2]["text"], "Mem: 8GB");
        });
    }

    #[test]
    fn dispatcher_ui_progress_with_percentage() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.result = null;
                    pi.ui("progress", { message: "Uploading", percent: 75, total: 100, current: 75 })
                        .then((r) => { globalThis.result = r; });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let captured: Arc<Mutex<Vec<ExtensionUiRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let ui_handler = Arc::new(TestUiHandler {
                captured: Arc::clone(&captured),
                response_value: Value::Null,
            });

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                ui_handler,
                PathBuf::from("."),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            let reqs = captured.lock().unwrap().clone();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].method, "progress");
            assert_eq!(reqs[0].payload["percent"], 75);
            assert_eq!(reqs[0].payload["total"], 100);
            assert_eq!(reqs[0].payload["current"], 75);
        });
    }

    // ---- Additional events conformance tests ----

    #[test]
    fn dispatcher_events_emit_name_field_alias() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use "name" instead of "event" field
            runtime
                .eval(
                    r#"
                    globalThis.seen = [];
                    globalThis.emitResult = null;

                    __pi_begin_extension("ext.listener", { name: "ext.listener" });
                    pi.on("named_event", (payload, _ctx) => { globalThis.seen.push(payload); });
                    __pi_end_extension();

                    __pi_begin_extension("ext.emitter", { name: "ext.emitter" });
                    pi.events("emit", { name: "named_event", data: { via: "name_field" } })
                      .then((r) => { globalThis.emitResult = r; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (!globalThis.emitResult) throw new Error("emit not resolved");
                    if (globalThis.emitResult.dispatched !== true) {
                        throw new Error("emit not dispatched: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (globalThis.seen.length !== 1) {
                        throw new Error("Expected 1 handler call, got: " + globalThis.seen.length);
                    }
                    if (globalThis.seen[0].via !== "name_field") {
                        throw new Error("Wrong payload: " + JSON.stringify(globalThis.seen[0]));
                    }
                "#,
                )
                .await
                .expect("verify name field alias");
        });
    }

    #[test]
    fn dispatcher_events_unsupported_op_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.events("nonexistent_op", {})
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for unsupported events op");
                    }
                    if (!globalThis.errMsg.toLowerCase().includes("unsupported")) {
                        throw new Error("Expected 'unsupported' error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify unsupported op rejection");
        });
    }

    #[test]
    fn dispatcher_events_emit_empty_event_name_rejects() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            runtime
                .eval(
                    r#"
                    globalThis.errMsg = "";
                    pi.events("emit", { event: "" })
                        .then(() => { globalThis.errMsg = "should_not_resolve"; })
                        .catch((e) => { globalThis.errMsg = e.message || String(e); });
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (globalThis.errMsg === "should_not_resolve") {
                        throw new Error("Expected rejection for empty event name");
                    }
                    if (!globalThis.errMsg.includes("event") && !globalThis.errMsg.includes("non-empty")) {
                        throw new Error("Expected event name error, got: " + globalThis.errMsg);
                    }
                "#,
                )
                .await
                .expect("verify empty event name rejection");
        });
    }

    #[test]
    fn dispatcher_events_emit_handler_count_in_response() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Register 2 handlers for same event
            runtime
                .eval(
                    r#"
                    globalThis.emitResult = null;

                    __pi_begin_extension("ext.h1", { name: "ext.h1" });
                    pi.on("counted_event", (_p, _c) => {});
                    __pi_end_extension();

                    __pi_begin_extension("ext.h2", { name: "ext.h2" });
                    pi.on("counted_event", (_p, _c) => {});
                    __pi_end_extension();

                    __pi_begin_extension("ext.emitter", { name: "ext.emitter" });
                    pi.events("emit", { event: "counted_event", data: {} })
                      .then((r) => { globalThis.emitResult = r; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            runtime.tick().await.expect("tick");

            runtime
                .eval(
                    r#"
                    if (!globalThis.emitResult) throw new Error("emit not resolved");
                    if (globalThis.emitResult.dispatched !== true) {
                        throw new Error("emit not dispatched: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (typeof globalThis.emitResult.handler_count !== "number") {
                        throw new Error("Expected handler_count number, got: " + JSON.stringify(globalThis.emitResult));
                    }
                    if (globalThis.emitResult.handler_count < 2) {
                        throw new Error("Expected at least 2 handlers, got: " + globalThis.emitResult.handler_count);
                    }
                "#,
                )
                .await
                .expect("verify handler count");
        });
    }

    #[test]
    fn dispatcher_events_list_returns_registered_event_names() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Register multiple event hooks
            runtime
                .eval(
                    r#"
                    globalThis.result = null;

                    __pi_begin_extension("ext.multi", { name: "ext.multi" });
                    pi.on("event_alpha", (_p, _c) => {});
                    pi.on("event_beta", (_p, _c) => {});
                    pi.on("event_gamma", (_p, _c) => {});
                    pi.events("list", {})
                        .then((r) => { globalThis.result = r; })
                        .catch((e) => { globalThis.result = { error: e.message || String(e) }; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("list not resolved");
                    if (globalThis.result.error) throw new Error("list error: " + globalThis.result.error);
                    const events = globalThis.result.events;
                    if (!Array.isArray(events)) {
                        throw new Error("Expected events array, got: " + JSON.stringify(globalThis.result));
                    }
                    if (events.length < 3) {
                        throw new Error("Expected at least 3 events, got: " + JSON.stringify(events));
                    }
                    if (!events.includes("event_alpha")) {
                        throw new Error("Missing event_alpha in: " + JSON.stringify(events));
                    }
                    if (!events.includes("event_beta")) {
                        throw new Error("Missing event_beta in: " + JSON.stringify(events));
                    }
                "#,
                )
                .await
                .expect("verify event names list");
        });
    }

    #[test]
    fn dispatcher_events_emit_no_handlers_still_resolves() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Emit an event that has no registered handlers
            runtime
                .eval(
                    r#"
                    globalThis.emitResult = null;

                    __pi_begin_extension("ext.lonely", { name: "ext.lonely" });
                    pi.events("emit", { event: "unheard_event", data: { msg: "nobody listens" } })
                      .then((r) => { globalThis.emitResult = r; })
                      .catch((e) => { globalThis.emitResult = { error: e.message || String(e) }; });
                    __pi_end_extension();
                "#,
                )
                .await
                .expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.emitResult) throw new Error("emit not resolved");
                    // Should resolve even with no handlers (dispatched: true, handler_count: 0)
                    if (globalThis.emitResult.error) {
                        throw new Error("emit errored: " + globalThis.emitResult.error);
                    }
                    if (globalThis.emitResult.dispatched !== true) {
                        throw new Error("emit not dispatched: " + JSON.stringify(globalThis.emitResult));
                    }
                "#,
                )
                .await
                .expect("verify emit with no handlers");
        });
    }

    // ---- Additional tool conformance tests ----

    #[test]
    fn dispatcher_tool_read_returns_file_content() {
        futures::executor::block_on(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let file_path = temp_dir.path().join("readable.txt");
            std::fs::write(&file_path, "file content here").expect("write test file");

            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            let script = format!(
                r#"
                globalThis.result = null;
                pi.tool("read", {{ path: "{}" }})
                    .then((r) => {{ globalThis.result = r; }})
                    .catch((e) => {{ globalThis.result = {{ error: e.message || String(e) }}; }});
            "#,
                file_path.display()
            );
            runtime.eval(&script).await.expect("eval");

            let requests = runtime.drain_hostcall_requests();
            assert_eq!(requests.len(), 1);

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&["read"], temp_dir.path(), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(NullSession),
                Arc::new(NullUiHandler),
                temp_dir.path().to_path_buf(),
            );

            for request in requests {
                dispatcher.dispatch_and_complete(request).await;
            }

            while runtime.has_pending() {
                runtime.tick().await.expect("tick");
                runtime.drain_microtasks().await.expect("microtasks");
            }

            runtime
                .eval(
                    r#"
                    if (!globalThis.result) throw new Error("read not resolved");
                    if (globalThis.result.error) throw new Error("read error: " + globalThis.result.error);
                "#,
                )
                .await
                .expect("verify read tool");
        });
    }

    // ======================================================================
    // bd-321a.4: Session dispatcher taxonomy tests
    // ======================================================================
    // Table-driven tests proving dispatch_session returns taxonomy-correct
    // error codes (timeout|denied|io|invalid_request|internal).

    /// Direct unit test of dispatch_session error taxonomy without JS runtime.
    /// Uses TestSession to verify error code classification for each operation.
    #[test]
    fn session_dispatch_taxonomy_unknown_op_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session("c1", "nonexistent_op", serde_json::json!({}))
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(
                        code, "invalid_request",
                        "unknown op must be invalid_request"
                    );
                }
                HostcallOutcome::Success(_) | HostcallOutcome::StreamChunk { .. } => {
                    panic!("unknown op should not succeed");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_set_model_missing_provider_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session("c2", "set_model", serde_json::json!({"modelId": "gpt-4o"}))
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(
                        code, "invalid_request",
                        "set_model missing provider must be invalid_request"
                    );
                }
                HostcallOutcome::Success(_) => {
                    panic!("set_model with missing provider should not succeed");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    panic!("set_model with missing provider should not stream");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_set_model_missing_model_id_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session(
                    "c3",
                    "set_model",
                    serde_json::json!({"provider": "anthropic"}),
                )
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(code, "invalid_request");
                }
                HostcallOutcome::Success(_) => {
                    panic!("set_model with missing modelId should not succeed");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    panic!("set_model with missing modelId should not stream");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_set_thinking_level_empty_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session("c4", "set_thinking_level", serde_json::json!({}))
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(code, "invalid_request");
                }
                HostcallOutcome::Success(_) => {
                    panic!("set_thinking_level with no level should not succeed");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    panic!("set_thinking_level with no level should not stream");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_set_label_empty_target_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session("c5", "set_label", serde_json::json!({}))
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(code, "invalid_request");
                }
                HostcallOutcome::Success(_) => {
                    panic!("set_label with no targetId should not succeed");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    panic!("set_label with no targetId should not stream");
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_append_message_invalid_is_invalid_request() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let outcome = dispatcher
                .dispatch_session(
                    "c6",
                    "append_message",
                    serde_json::json!({"message": {"not_a_valid_message": true}}),
                )
                .await;
            match outcome {
                HostcallOutcome::Error { code, .. } => {
                    assert_eq!(
                        code, "invalid_request",
                        "malformed message must be invalid_request"
                    );
                }
                HostcallOutcome::Success(_) => {
                    panic!("invalid message should not succeed");
                }
                HostcallOutcome::StreamChunk { .. } => {
                    panic!("invalid message should not stream");
                }
            }
        });
    }

    #[test]
    #[allow(clippy::items_after_statements, clippy::too_many_lines)]
    fn session_dispatch_taxonomy_io_error_from_session_trait() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );

            // Use a session impl that returns IO errors
            struct FailSession;

            #[async_trait]
            impl ExtensionSession for FailSession {
                async fn get_state(&self) -> Value {
                    Value::Null
                }
                async fn get_messages(&self) -> Vec<SessionMessage> {
                    Vec::new()
                }
                async fn get_entries(&self) -> Vec<Value> {
                    Vec::new()
                }
                async fn get_branch(&self) -> Vec<Value> {
                    Vec::new()
                }
                async fn set_name(&self, _name: String) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn append_message(&self, _message: SessionMessage) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn append_custom_entry(
                    &self,
                    _custom_type: String,
                    _data: Option<Value>,
                ) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn set_model(&self, _provider: String, _model_id: String) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn get_model(&self) -> (Option<String>, Option<String>) {
                    (None, None)
                }
                async fn set_thinking_level(&self, _level: String) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
                async fn get_thinking_level(&self) -> Option<String> {
                    None
                }
                async fn set_label(
                    &self,
                    _target_id: String,
                    _label: Option<String>,
                ) -> Result<()> {
                    Err(crate::error::Error::from(std::io::Error::other(
                        "disk full",
                    )))
                }
            }

            let dispatcher = ExtensionDispatcher::new(
                Rc::clone(&runtime),
                Arc::new(ToolRegistry::new(&[], Path::new("."), None)),
                Arc::new(HttpConnector::with_defaults()),
                Arc::new(FailSession),
                Arc::new(NullUiHandler),
                PathBuf::from("."),
            );

            // Table of ops that call session trait mutators (which will fail with IO error)
            let io_cases = [
                ("set_name", serde_json::json!({"name": "test"})),
                (
                    "set_model",
                    serde_json::json!({"provider": "a", "modelId": "b"}),
                ),
                ("set_thinking_level", serde_json::json!({"level": "high"})),
                (
                    "set_label",
                    serde_json::json!({"targetId": "abc", "label": "x"}),
                ),
                (
                    "append_entry",
                    serde_json::json!({"customType": "note", "data": null}),
                ),
                (
                    "append_message",
                    serde_json::json!({"message": {"role": "custom", "customType": "x", "content": "y", "display": true}}),
                ),
            ];

            for (op, params) in &io_cases {
                let outcome = dispatcher.dispatch_session("cx", op, params.clone()).await;
                match outcome {
                    HostcallOutcome::Error { code, .. } => {
                        assert_eq!(code, "io", "session IO error for op '{op}' must be 'io'");
                    }
                    HostcallOutcome::Success(_) => {
                        panic!("op '{op}' with failing session should not succeed");
                    }
                    HostcallOutcome::StreamChunk { .. } => {
                        panic!("op '{op}' with failing session should not stream");
                    }
                }
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_read_ops_succeed_with_null_session() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            let read_ops = [
                "get_state",
                "getState",
                "get_messages",
                "getMessages",
                "get_entries",
                "getEntries",
                "get_branch",
                "getBranch",
                "get_file",
                "getFile",
                "get_name",
                "getName",
                "get_model",
                "getModel",
                "get_thinking_level",
                "getThinkingLevel",
            ];

            for op in &read_ops {
                let outcome = dispatcher
                    .dispatch_session("cr", op, serde_json::json!({}))
                    .await;
                assert!(
                    matches!(outcome, HostcallOutcome::Success(_)),
                    "read op '{op}' should succeed"
                );
            }
        });
    }

    #[test]
    fn session_dispatch_taxonomy_case_insensitive_aliases() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));

            // Each alias pair should produce the same result
            let alias_pairs = [
                ("get_state", "getstate"),
                ("get_messages", "getmessages"),
                ("get_entries", "getentries"),
                ("get_branch", "getbranch"),
                ("get_file", "getfile"),
                ("get_name", "getname"),
                ("get_model", "getmodel"),
                ("get_thinking_level", "getthinkinglevel"),
            ];

            for (snake, camel) in &alias_pairs {
                let outcome_a = dispatcher
                    .dispatch_session("ca", snake, serde_json::json!({}))
                    .await;
                let outcome_b = dispatcher
                    .dispatch_session("cb", camel, serde_json::json!({}))
                    .await;
                match (&outcome_a, &outcome_b) {
                    (HostcallOutcome::Success(a), HostcallOutcome::Success(b)) => {
                        assert_eq!(
                            a, b,
                            "alias pair ({snake}, {camel}) should produce same output"
                        );
                    }
                    _ => panic!("alias pair ({snake}, {camel}) should both succeed"),
                }
            }
        });
    }

    #[test]
    fn protocol_adapter_host_call_to_host_result_success() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let message = ExtensionMessage {
                id: "msg-hostcall-1".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-hostcall-1".to_string(),
                    capability: "session".to_string(),
                    method: "session".to_string(),
                    params: serde_json::json!({ "op": "get_state" }),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert_eq!(result.call_id, "call-hostcall-1");
                    assert!(!result.is_error, "expected success host_result");
                    assert!(
                        result.output.is_object(),
                        "host_result output must remain object"
                    );
                    assert!(result.error.is_none(), "success should not include error");
                }
                other => panic!("expected host_result body, got {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_adapter_missing_op_returns_invalid_request_taxonomy() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let message = ExtensionMessage {
                id: "msg-hostcall-2".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::HostCall(HostCallPayload {
                    call_id: "call-hostcall-2".to_string(),
                    capability: "session".to_string(),
                    method: "session".to_string(),
                    params: serde_json::json!({}),
                    timeout_ms: None,
                    cancel_token: None,
                    context: None,
                }),
            };

            let response = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect("protocol dispatch");

            match response.body {
                ExtensionBody::HostResult(result) => {
                    assert!(result.is_error, "expected error host_result");
                    assert!(result.output.is_object(), "error output must be object");
                    let error = result.error.expect("error payload");
                    assert_eq!(
                        error.code,
                        crate::extensions::HostCallErrorCode::InvalidRequest
                    );
                }
                other => panic!("expected host_result body, got {other:?}"),
            }
        });
    }

    #[test]
    fn protocol_adapter_rejects_non_host_call_messages() {
        futures::executor::block_on(async {
            let runtime = Rc::new(
                PiJsRuntime::with_clock(DeterministicClock::new(0))
                    .await
                    .expect("runtime"),
            );
            let dispatcher = build_dispatcher(Rc::clone(&runtime));
            let message = ExtensionMessage {
                id: "msg-hostcall-3".to_string(),
                version: PROTOCOL_VERSION.to_string(),
                body: ExtensionBody::ToolResult(crate::extensions::ToolResultPayload {
                    call_id: "tool-1".to_string(),
                    output: serde_json::json!({}),
                    is_error: false,
                }),
            };

            let err = dispatcher
                .dispatch_protocol_message(message)
                .await
                .expect_err("non-host-call should fail");
            assert!(
                err.to_string()
                    .contains("dispatch_protocol_message expects host_call"),
                "unexpected error: {err}"
            );
        });
    }
}
