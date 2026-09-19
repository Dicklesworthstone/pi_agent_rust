//! DAP session state, launch sequencing and retained breakpoint configuration.

mod execution;
mod inspection;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{Either, select};
use serde_json::{Value, json};

use super::breakpoints::{self, Change, Group, Store};
use super::dap::{DapError, DapEvent, DapTransport};
use super::tool_err;
use crate::agent_cx::AgentCx;
use crate::error::Result;
use execution::Execution;

pub const DEFAULT_DAP_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const MAX_INSPECTION_HANDLE: u64 = inspection::MAX_HANDLE;
const INITIALIZED_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExecState {
    Running,
    Stopped { thread_id: u64, reason: String },
    Exited,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Launch,
    Attach,
}

struct State {
    execution: Execution,
    initialized: bool,
    capabilities: Value,
    origin: Option<Origin>,
    fault: Option<String>,
    stop_wait: Option<(u64, Option<u64>)>,
    inspection_revision: u64,
}

impl State {
    fn invalidate_inspection(&mut self) -> Result<()> {
        self.inspection_revision = self
            .inspection_revision
            .checked_add(1)
            .ok_or_else(|| tool_err("DAP_PROTOCOL", "inspection revision exhausted"))?;
        Ok(())
    }

    fn event(&mut self, event: &DapEvent) -> Result<()> {
        match event.event.as_str() {
            "initialized" => self.initialized = true,
            // Area/thread/frame IDs are advisory hints. Conservatively expire
            // all model-facing handles rather than retaining a stale subset.
            "invalidated" => self.invalidate_inspection()?,
            "capabilities" => {
                if let (Some(current), Some(update)) = (
                    self.capabilities.as_object_mut(),
                    event.body["capabilities"].as_object(),
                ) {
                    current.extend(update.clone());
                }
            }
            _ => self.execution.event(&event.event, &event.body)?,
        }
        Ok(())
    }

    fn check(&self) -> Result<()> {
        if let Some(fault) = &self.fault {
            return Err(tool_err("DAP_PROTOCOL", fault.clone()));
        }
        Ok(())
    }
}

/// Field order matters: stop the owned adapter before removing build outputs.
pub struct DapSession {
    transport: DapTransport,
    state: Mutex<State>,
    pub(super) breakpoints: Arc<asupersync::sync::Mutex<Store>>,
    inspection: Arc<asupersync::sync::Mutex<inspection::Handles>>,
    launch_artifacts: Option<tempfile::TempDir>,
}

impl DapSession {
    pub async fn begin(transport: DapTransport) -> Result<Self> {
        let capabilities = transport.request("initialize", json!({
            "clientID": "pi_agent_rust", "clientName": "pi_agent_rust", "adapterID": "pi-dap",
            "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path",
            "supportsVariableType": true, "supportsVariablePaging": true,
            "supportsInvalidatedEvent": true,
            "supportsRunInTerminalRequest": false, "supportsStartDebuggingRequest": false
        }), DEFAULT_DAP_TIMEOUT).await?;
        if !capabilities.is_object() {
            return Err(tool_err(
                "DAP_PROTOCOL",
                "initialize did not return adapter capabilities",
            ));
        }
        Ok(Self {
            transport,
            state: Mutex::new(State {
                execution: Execution::default(),
                initialized: false,
                capabilities,
                origin: None,
                fault: None,
                stop_wait: None,
                inspection_revision: 0,
            }),
            breakpoints: Arc::new(asupersync::sync::Mutex::new(Store::default())),
            inspection: Arc::new(asupersync::sync::Mutex::new(inspection::Handles::default())),
            launch_artifacts: None,
        })
    }

    pub(super) fn with_launch_artifacts(mut self, directory: Option<tempfile::TempDir>) -> Self {
        self.launch_artifacts = directory;
        self
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[must_use]
    pub fn capabilities(&self) -> Value {
        self.pump_events();
        Self::lock(&self.state).capabilities.clone()
    }

    pub(super) fn require_capability(&self, name: &str) -> Result<()> {
        if self.capabilities()[name] == true {
            Ok(())
        } else {
            Err(tool_err(
                "DAP_UNSUPPORTED",
                format!("adapter does not advertise {name}"),
            ))
        }
    }

    #[must_use]
    pub fn state(&self) -> ExecState {
        self.pump_events();
        Self::lock(&self.state).execution.aggregate()
    }

    #[must_use]
    pub fn execution_snapshot(&self) -> Value {
        self.pump_events();
        let (mut snapshot, inspection_revision, fault) = {
            let state = Self::lock(&self.state);
            (
                state.execution.snapshot(),
                state.inspection_revision,
                state.fault.clone(),
            )
        };
        snapshot["inspectionRevision"] = json!(inspection_revision);
        if let Some(fault) = &fault {
            snapshot["fault"] = json!(fault);
        }
        snapshot
    }

    pub(super) fn invalidate_inspection(&self) -> Result<()> {
        Self::lock(&self.state).invalidate_inspection()
    }

    #[must_use]
    pub fn output_tail(&self) -> String {
        self.pump_events();
        self.transport.stderr_tail()
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive() && self.state() != ExecState::Exited
    }

    pub(super) fn is_connected(&self) -> bool {
        self.transport.is_alive()
    }

    pub fn pump_events(&self) {
        let failed = {
            let mut state = Self::lock(&self.state);
            for event in self.transport.drain_events() {
                if let Err(error) = state.event(&event) {
                    state.fault = Some(error.to_string());
                    state.execution.exit();
                    break;
                }
            }
            if !self.transport.is_alive() {
                state.execution.exit();
            }
            state.fault.is_some()
        };
        if failed {
            self.transport.kill();
        }
    }

    /// One startup budget covers compilation and initial configuration.
    pub(super) async fn start(
        &self,
        command: &str,
        arguments: Value,
        initial: &BTreeMap<String, Vec<Value>>,
        exception_filters: Option<&[String]>,
        timeout: Duration,
    ) -> Result<()> {
        let origin = match command {
            "launch" => Origin::Launch,
            "attach" => Origin::Attach,
            _ => return Err(tool_err("DAP_USAGE", "startup must be launch or attach")),
        };
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(tool_err(
                "DAP_USAGE",
                "startup timeout must be in 1..=300000 ms",
            ));
        }
        Self::lock(&self.state).origin = Some(origin);
        let configure = async {
            self.wait_initialized_for(timeout).await?;
            for (path, entries) in initial {
                for entry in entries {
                    breakpoints::check_options(self, entry)?;
                }
                breakpoints::apply(
                    self,
                    Group::Source(path.clone()),
                    Change::Replace(entries.clone()),
                )
                .await?;
            }
            if let Some(filters) = exception_filters {
                let caps = self.capabilities();
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
                self.call("setExceptionBreakpoints", json!({"filters": filters}))
                    .await?;
            }
            if self.capabilities()["supportsConfigurationDoneRequest"] == true {
                self.call("configurationDone", json!({})).await?;
            }
            Ok::<(), crate::error::Error>(())
        };
        let operation = async {
            let launch = async {
                self.transport
                    .request(command, arguments, timeout)
                    .await
                    .map_err(crate::error::Error::from)
            };
            futures::future::try_join(launch, configure).await?;
            self.pump_events();
            Self::lock(&self.state).check()?;
            Ok(())
        };
        let owner = AgentCx::for_current_or_request();
        let deadline = async {
            owner.time().sleep(timeout).await;
        };
        match select(Box::pin(operation), Box::pin(deadline)).await {
            Either::Left((result, _)) => result,
            Either::Right(((), pending)) => {
                drop(pending);
                Err(tool_err(
                    "DAP_STARTUP_TIMEOUT",
                    "debug launch/attach configuration exceeded its startup budget",
                ))
            }
        }
    }

    pub async fn wait_initialized(&self) -> Result<()> {
        self.wait_initialized_for(INITIALIZED_WAIT).await
    }

    async fn wait_initialized_for(&self, wait: Duration) -> Result<()> {
        let owner = AgentCx::for_current_or_request();
        let start = owner
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        loop {
            owner
                .checkpoint()
                .map_err(|_| tool_err("DAP_CANCELLED", "debug configuration cancelled"))?;
            self.pump_events();
            {
                let state = Self::lock(&self.state);
                state.check()?;
                if state.initialized {
                    return Ok(());
                }
                if state.execution.aggregate() == ExecState::Exited {
                    return Err(tool_err(
                        "DAP_TRANSPORT",
                        "adapter ended before initialization",
                    ));
                }
            }
            let now = owner
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            if Duration::from_nanos(now.duration_since(start)) >= wait {
                return Err(tool_err(
                    "DAP_INITIALIZE_TIMEOUT",
                    "adapter did not emit initialized",
                ));
            }
            owner.time().sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn wait_stopped(&self, wait: Duration) -> Option<(u64, String)> {
        let owner = AgentCx::for_current_or_request();
        let start = owner
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        loop {
            if owner.checkpoint().is_err() {
                return None;
            }
            self.pump_events();
            let stopped = {
                let state = Self::lock(&self.state);
                if state.execution.aggregate() == ExecState::Exited {
                    return None;
                }
                if let Some((revision, thread)) = state.stop_wait {
                    state.execution.stopped_since(revision, thread)
                } else if let ExecState::Stopped { thread_id, reason } = state.execution.aggregate()
                {
                    Some((thread_id, reason))
                } else {
                    None
                }
            };
            if let Some(stop) = stopped {
                return Some(stop);
            }
            let now = owner
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            if Duration::from_nanos(now.duration_since(start)) >= wait {
                return None;
            }
            owner.time().sleep(Duration::from_millis(10)).await;
        }
    }

    pub(super) fn require_stopped(&self) -> Result<u64> {
        self.require_thread(None)
    }

    pub(super) fn require_thread(&self, thread: Option<u64>) -> Result<u64> {
        self.pump_events();
        let state = Self::lock(&self.state);
        state.check()?;
        state.execution.require(thread)
    }

    pub async fn call(&self, command: &str, arguments: Value) -> Result<Value> {
        self.pump_events();
        let resuming = matches!(
            command,
            "continue"
                | "next"
                | "stepIn"
                | "stepOut"
                | "stepBack"
                | "reverseContinue"
                | "restartFrame"
        );
        let single = match arguments.get("singleThread") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| tool_err("DAP_USAGE", "singleThread must be boolean"))?,
        };
        if single && resuming {
            self.require_capability("supportsSingleThreadExecutionRequests")?;
        }
        let (previous, revision, previous_wait, own_wait) = {
            let mut state = Self::lock(&self.state);
            state.check()?;
            let previous_wait = state.stop_wait;
            let previous = if resuming {
                let thread = state
                    .execution
                    .require(arguments.get("threadId").and_then(Value::as_u64))?;
                Some(state.execution.resume(thread, single)?)
            } else {
                None
            };
            let revision = state.execution.revision();
            let own_wait = if resuming || command == "pause" {
                Some((
                    revision,
                    if single || command == "pause" {
                        arguments.get("threadId").and_then(Value::as_u64)
                    } else {
                        None
                    },
                ))
            } else {
                None
            };
            if own_wait.is_some() {
                state.stop_wait = own_wait;
            }
            drop(state);
            (previous, revision, previous_wait, own_wait)
        };
        let result = self
            .transport
            .request(command, arguments, DEFAULT_DAP_TIMEOUT)
            .await;
        self.pump_events();
        let mut state = Self::lock(&self.state);
        state.check()?;
        if matches!(&result, Err(DapError::Adapter { .. })) {
            if let Some(previous) = &previous {
                state.execution.restore(previous, false);
            }
            if own_wait.is_some() && state.stop_wait == own_wait {
                state.stop_wait = previous_wait;
            }
        }
        if let Ok(body) = &result {
            if command == "threads" {
                state.execution.observe_threads(body, revision)?;
            }
            if command == "continue"
                && let Some(previous) = &previous
            {
                match body.get("allThreadsContinued") {
                    Some(Value::Bool(false)) => state.execution.restore(previous, true),
                    None | Some(Value::Bool(true)) => state.execution.continued_all(previous),
                    _ => {
                        return Err(tool_err(
                            "DAP_PROTOCOL",
                            "allThreadsContinued reply must be boolean",
                        ));
                    }
                }
            }
        }
        result.map_err(crate::error::Error::from)
    }

    /// Typed stack/object inspection translates opaque local handles to the
    /// adapter's IDs. Do not mix these handles with raw `call` replies.
    pub async fn call_stopped(&self, command: &str, arguments: Value) -> Result<Value> {
        if matches!(
            command,
            "stackTrace"
                | "scopes"
                | "variables"
                | "evaluate"
                | "setVariable"
                | "setExpression"
                | "exceptionInfo"
                | "dataBreakpointInfo"
        ) {
            return inspection::call(self, command, arguments).await;
        }
        self.require_thread(arguments.get("threadId").and_then(Value::as_u64))?;
        self.call(command, arguments).await
    }

    pub(super) async fn disconnect(&self, terminate_debuggee: bool) -> Result<()> {
        let capabilities = self.capabilities();
        let origin = Self::lock(&self.state).origin;
        let arguments = disconnect_arguments(origin, &capabilities, terminate_debuggee)?;
        self.transport
            .request("disconnect", arguments, DEFAULT_DAP_TIMEOUT)
            .await?;
        self.transport.kill();
        Self::lock(&self.state).execution.exit();
        Ok(())
    }

    pub async fn terminate(&self) {
        let _ = self.call("terminate", json!({})).await;
        self.transport.kill();
        Self::lock(&self.state).execution.exit();
    }
}

fn disconnect_arguments(origin: Option<Origin>, caps: &Value, terminate: bool) -> Result<Value> {
    let origin = origin.ok_or_else(|| {
        tool_err(
            "DAP_NO_SESSION",
            "debug session did not complete a start request",
        )
    })?;
    if !terminate && origin == Origin::Launch {
        return Err(tool_err(
            "DAP_USAGE",
            "disconnect preserves attached targets only; use terminate for a Pi-launched program",
        ));
    }
    if terminate && origin == Origin::Attach && caps["supportTerminateDebuggee"] != true {
        return Err(tool_err(
            "DAP_UNSUPPORTED",
            "adapter cannot guarantee the requested termination of an attached target; disconnect to leave it running",
        ));
    }
    let mut args = json!({"restart":false});
    if caps["supportTerminateDebuggee"] == true {
        args["terminateDebuggee"] = json!(terminate);
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            execution: Execution::default(),
            initialized: false,
            capabilities: json!({}),
            origin: None,
            fault: None,
            stop_wait: None,
            inspection_revision: 0,
        }
    }

    #[test]
    fn state_labels() {
        let stopped = ExecState::Stopped {
            thread_id: 7,
            reason: "breakpoint".to_string(),
        };
        let rendered = serde_json::to_string(&stopped).expect("serialize");
        assert!(rendered.contains("\"state\":\"stopped\""));
        assert!(rendered.contains("\"thread_id\":7"));
    }

    #[test]
    fn initialized_and_stopped_in_the_same_batch_are_both_retained() {
        let mut state = state();
        state
            .event(&DapEvent {
                event: "initialized".into(),
                body: json!({}),
            })
            .unwrap();
        state
            .event(&DapEvent {
                event: "stopped".into(),
                body: json!({"threadId":7,"reason":"entry"}),
            })
            .unwrap();
        assert!(state.initialized);
        assert!(matches!(
            state.execution.aggregate(),
            ExecState::Stopped { thread_id: 7, .. }
        ));
        state
            .event(&DapEvent {
                event: "continued".into(),
                body: json!({}),
            })
            .unwrap();
        assert!(state.initialized);
        assert_eq!(state.execution.aggregate(), ExecState::Running);
    }

    #[test]
    fn terminal_state_is_not_resurrected_and_capabilities_merge() {
        let mut state = state();
        state
            .event(&DapEvent {
                event: "capabilities".into(),
                body: json!({"capabilities":{"supportsLogPoints":true}}),
            })
            .unwrap();
        assert_eq!(state.capabilities["supportsLogPoints"], true);
        state
            .event(&DapEvent {
                event: "exited".into(),
                body: json!({}),
            })
            .unwrap();
        state
            .event(&DapEvent {
                event: "stopped".into(),
                body: json!({"threadId":3}),
            })
            .unwrap();
        assert_eq!(state.execution.aggregate(), ExecState::Exited);
    }

    #[test]
    fn invalidated_refreshes_handles_without_manufacturing_a_new_stop() {
        let mut state = state();
        state
            .event(&DapEvent {
                event: "stopped".into(),
                body: json!({"threadId":7,"reason":"entry"}),
            })
            .unwrap();
        let stop = state.execution.stamp(7);
        state
            .event(&DapEvent {
                event: "invalidated".into(),
                body: json!({"areas":["variables"],"threadId":7}),
            })
            .unwrap();
        assert_eq!(state.execution.stamp(7), stop);
        assert_eq!(state.inspection_revision, 1);
    }

    #[test]
    fn disconnect_respects_target_origin_and_optional_capabilities() {
        assert_eq!(
            disconnect_arguments(Some(Origin::Launch), &json!({}), true).unwrap(),
            json!({"restart":false})
        );
        assert_eq!(
            disconnect_arguments(Some(Origin::Attach), &json!({}), false).unwrap(),
            json!({"restart":false})
        );
        assert!(disconnect_arguments(Some(Origin::Attach), &json!({}), true).is_err());
        assert!(
            disconnect_arguments(
                Some(Origin::Launch),
                &json!({"supportTerminateDebuggee":true}),
                false
            )
            .is_err()
        );
        assert_eq!(
            disconnect_arguments(
                Some(Origin::Attach),
                &json!({"supportTerminateDebuggee":true}),
                true
            )
            .unwrap()["terminateDebuggee"],
            true
        );
        assert_eq!(
            disconnect_arguments(
                Some(Origin::Attach),
                &json!({"supportTerminateDebuggee":true}),
                false
            )
            .unwrap()["terminateDebuggee"],
            false
        );
    }
}
