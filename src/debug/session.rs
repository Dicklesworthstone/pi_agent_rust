//! DAP session: the stopped/running state machine over one adapter
//! transport, with typed requests for the debug surface (bd-cv653.1.2).

use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use super::dap::DapTransport;

/// Default per-request timeout.
pub const DEFAULT_DAP_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounded wait for the adapter's `initialized` event after launch/attach.
const INITIALIZED_WAIT: Duration = Duration::from_secs(10);

fn tool_err(code: &str, message: impl Into<String>) -> crate::error::Error {
    crate::error::Error::tool("debug", format!("[{code}] {}", message.into()))
}

/// The debuggee's execution state.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExecState {
    /// Program is running (or not yet started).
    Running,
    /// Stopped: reason + the thread that stopped.
    Stopped { thread_id: u64, reason: String },
    /// The debugee exited or the adapter ended the session.
    Exited,
}

/// One live debug session.
pub struct DapSession {
    transport: DapTransport,
    state: Mutex<ExecState>,
    /// Adapter capabilities from the initialize response.
    capabilities: Mutex<Value>,
}

impl DapSession {
    /// Wrap a spawned transport with the `initialize` handshake.
    ///
    /// # Errors
    ///
    /// Fails when the adapter rejects `initialize`.
    pub async fn begin(transport: DapTransport) -> crate::error::Result<Self> {
        let session = Self {
            transport,
            state: Mutex::new(ExecState::Running),
            capabilities: Mutex::new(Value::Null),
        };
        let caps = session
            .transport
            .request(
                "initialize",
                serde_json::json!({
                    "clientID": "pi_agent_rust",
                    "clientName": "pi_agent_rust",
                    "adapterID": "pi-dap",
                    "linesStartAt1": true,
                    "columnsStartAt1": true,
                    "pathFormat": "path",
                }),
                DEFAULT_DAP_TIMEOUT,
            )
            .await
            .map_err(crate::error::Error::from)?;
        *Self::lock(&session.capabilities) = caps;
        Ok(session)
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adapter capabilities.
    #[must_use]
    pub fn capabilities(&self) -> Value {
        Self::lock(&self.capabilities).clone()
    }

    /// Current execution state (events drained first).
    #[must_use]
    pub fn state(&self) -> ExecState {
        self.pump_events();
        Self::lock(&self.state).clone()
    }

    /// Stderr/output tail.
    #[must_use]
    pub fn output_tail(&self) -> String {
        self.pump_events();
        self.transport.stderr_tail()
    }

    /// Whether the adapter is still connected and the debuggee not exited.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive() && !matches!(self.state(), ExecState::Exited)
    }

    /// Merge adapter events into the state machine.
    pub fn pump_events(&self) {
        for event in self.transport.drain_events() {
            match event.event.as_str() {
                "stopped" => {
                    let thread_id = event
                        .body
                        .get("threadId")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let reason = event
                        .body
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    *Self::lock(&self.state) = ExecState::Stopped { thread_id, reason };
                }
                "continued" => {
                    *Self::lock(&self.state) = ExecState::Running;
                }
                "terminated" | "exited" => {
                    *Self::lock(&self.state) = ExecState::Exited;
                }
                _ => {}
            }
        }
    }

    /// Bounded wait until the debuggee is stopped (or already stopped).
    pub async fn wait_stopped(&self, wait: Duration) -> Option<(u64, String)> {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let start = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        loop {
            self.pump_events();
            let current = { Self::lock(&self.state).clone() };
            if let ExecState::Stopped { thread_id, reason } = current {
                return Some((thread_id, reason));
            }
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            if std::time::Duration::from_nanos(now.duration_since(start)) >= wait {
                return None;
            }
            asupersync::time::sleep(now, Duration::from_millis(10)).await;
        }
    }

    /// Bounded wait for the `initialized` event (fired by the adapter after
    /// launch/attach is accepted).
    pub async fn wait_initialized(&self) -> bool {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let start = cx
            .cx()
            .timer_driver()
            .map_or_else(asupersync::time::wall_now, |timer| timer.now());
        loop {
            for event in self.transport.drain_events() {
                if event.event == "initialized" {
                    return true;
                }
                // Keep the state machine fed while we wait.
                match event.event.as_str() {
                    "stopped" => {
                        let thread_id = event
                            .body
                            .get("threadId")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let reason = event
                            .body
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        *Self::lock(&self.state) = ExecState::Stopped { thread_id, reason };
                    }
                    "continued" => *Self::lock(&self.state) = ExecState::Running,
                    "terminated" | "exited" => *Self::lock(&self.state) = ExecState::Exited,
                    _ => {}
                }
            }
            let now = cx
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            if std::time::Duration::from_nanos(now.duration_since(start)) >= INITIALIZED_WAIT {
                return false;
            }
            asupersync::time::sleep(now, Duration::from_millis(10)).await;
        }
    }

    /// Require the stopped state for stack/scoped operations.
    fn require_stopped(&self) -> crate::error::Result<u64> {
        match self.state() {
            ExecState::Stopped { thread_id, .. } => Ok(thread_id),
            ExecState::Running => Err(tool_err(
                "DAP_STATE_RUNNING",
                "debuggee is running; pause or wait for a breakpoint first",
            )),
            ExecState::Exited => Err(tool_err(
                "DAP_STATE_EXITED",
                "debuggee has exited; start a new session",
            )),
        }
    }

    /// A typed request with the DAP timeout.
    pub async fn call(&self, command: &str, arguments: Value) -> crate::error::Result<Value> {
        self.transport
            .request(command, arguments, DEFAULT_DAP_TIMEOUT)
            .await
            .map_err(crate::error::Error::from)
    }

    /// A state-gated request (requires stopped).
    pub async fn call_stopped(
        &self,
        command: &str,
        arguments: Value,
    ) -> crate::error::Result<Value> {
        self.require_stopped()?;
        self.call(command, arguments).await
    }

    /// Tear down: `terminate` request, then kill the adapter tree.
    pub async fn terminate(&self) {
        let _ = self.call("terminate", serde_json::json!({})).await;
        self.transport.kill();
        *Self::lock(&self.state) = ExecState::Exited;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
