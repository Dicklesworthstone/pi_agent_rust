//! Hostcall dispatcher for JS extensions.
//!
//! This module introduces the core `ExtensionDispatcher` abstraction used to route
//! hostcall requests (tools, HTTP, session, UI, etc.) from the JS runtime to
//! Rust implementations.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use asupersync::Cx;
use asupersync::channel::oneshot;
use async_trait::async_trait;
use serde_json::Value;

use crate::connectors::{Connector, HostCallPayload, http::HttpConnector};
use crate::error::Result;
use crate::extensions::{ExtensionSession, ExtensionUiRequest, ExtensionUiResponse};
use crate::extensions_js::{HostcallKind, HostcallRequest, PiJsRuntime};
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
    pub async fn dispatch_and_complete(&self, request: HostcallRequest) {
        let HostcallRequest {
            call_id,
            kind,
            payload,
            ..
        } = request;

        let outcome = match kind {
            HostcallKind::Tool { name } => self.dispatch_tool(&call_id, &name, payload).await,
            HostcallKind::Exec { cmd } => self.dispatch_exec(&call_id, &cmd, payload).await,
            HostcallKind::Http => self.dispatch_http(&call_id, payload).await,
            HostcallKind::Session { op } => self.dispatch_session(&call_id, &op, payload).await,
            HostcallKind::Ui { op } => self.dispatch_ui(&call_id, &op, payload).await,
            HostcallKind::Events { op } => self.dispatch_events(&call_id, &op, payload).await,
        };

        self.runtime.complete_hostcall(call_id, outcome);
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
                code: "tool_error".to_string(),
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
        use std::io::Read as _;
        use std::process::{Command, Stdio};

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
            .map(ToString::to_string);
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
                    .stderr(Stdio::piped());

                if let Some(cwd) = cwd.as_ref() {
                    command.current_dir(cwd);
                }

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

    #[allow(clippy::future_not_send)]
    async fn dispatch_session(&self, _call_id: &str, op: &str, payload: Value) -> HostcallOutcome {
        let op_norm = op.trim().to_ascii_lowercase();
        let result: std::result::Result<Value, String> = match op_norm.as_str() {
            "get_state" | "getstate" => Ok(self.session.get_state().await),
            "get_messages" | "getmessages" => {
                serde_json::to_value(self.session.get_messages().await)
                    .map_err(|err| format!("Serialize messages: {err}"))
            }
            "get_entries" | "getentries" => serde_json::to_value(self.session.get_entries().await)
                .map_err(|err| format!("Serialize entries: {err}")),
            "get_branch" | "getbranch" => serde_json::to_value(self.session.get_branch().await)
                .map_err(|err| format!("Serialize branch: {err}")),
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
                    .map_err(|err| err.to_string())
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
                    .map_err(|err| err.to_string())
            }
            "append_message" | "appendmessage" => {
                let message_value = payload.get("message").cloned().unwrap_or(payload);
                match serde_json::from_value(message_value) {
                    Ok(message) => self
                        .session
                        .append_message(message)
                        .await
                        .map(|()| Value::Null)
                        .map_err(|err| err.to_string()),
                    Err(err) => Err(format!("Parse message: {err}")),
                }
            }
            _ => Err(format!("Unknown session op: {op}")),
        };

        match result {
            Ok(value) => HostcallOutcome::Success(value),
            Err(message) => HostcallOutcome::Error {
                code: "invalid_request".to_string(),
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
            Ok(Some(response)) => HostcallOutcome::Success(response.value.unwrap_or(Value::Null)),
            Ok(None) => HostcallOutcome::Success(Value::Null),
            Err(err) => HostcallOutcome::Error {
                code: "io".to_string(),
                message: err.to_string(),
            },
        }
    }

    #[allow(clippy::future_not_send)]
    async fn dispatch_events(&self, _call_id: &str, op: &str, _payload: Value) -> HostcallOutcome {
        HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: format!("Unsupported events op: {op}"),
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
    use crate::scheduler::DeterministicClock;
    use crate::session::SessionMessage;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::Mutex;

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

    struct TestSession {
        state: Arc<Mutex<Value>>,
        messages: Arc<Mutex<Vec<SessionMessage>>>,
        entries: Arc<Mutex<Vec<Value>>>,
        branch: Arc<Mutex<Vec<Value>>>,
        name: Arc<Mutex<Option<String>>>,
        custom_entries: CustomEntries,
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
                    other => panic!("Unexpected message: {other:?}"),
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
}
