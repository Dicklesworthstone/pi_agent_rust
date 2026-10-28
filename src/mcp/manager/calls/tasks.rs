//! Task-augmented tools/call, owned by the same invocation as ordinary tools.
//!
//! A task receipt is not a tool result. Keep the original progress subscription
//! and deadline alive through polling and tasks/result, and never replay the
//! initiating call or move its task ID into a replacement protocol session.

use super::{
    AgentCx, Arc, Duration, Error, Instant, McpManager, McpTransport, RequestDispatch, Result,
    ServerEntry, ToolUpdate, Value, catalog, is_current, tool_err,
};
use crate::mcp::tasks::{Task, TaskCapabilities, TaskStatus, TaskSupport};
use crate::model::{ContentBlock, TextContent};
use serde_json::json;

const MAX_STATUS_UPDATES: usize = 128;

pub(super) struct Execution {
    augmented: bool,
    capabilities: TaskCapabilities,
    generation: u64,
}

impl Execution {
    pub(super) const fn is_augmented(&self) -> bool {
        self.augmented
    }

    pub(super) fn capture(
        transport: &Arc<dyn McpTransport>,
        support: TaskSupport,
        expected_generation: u64,
    ) -> Result<Self> {
        let generation = transport.progress_generation();
        let capabilities = transport.task_capabilities();
        if generation != expected_generation || transport.progress_generation() != generation {
            return Err(tool_err(
                "MCP_REQUEST_NOT_SENT",
                "protocol session changed after tool discovery; the call was not sent",
            ));
        }
        if support == TaskSupport::Required && !capabilities.tool_call {
            return Err(tool_err(
                "MCP_TASK_UNSUPPORTED",
                "the selected tool requires tasks, but this connection did not negotiate task-augmented tools/call; the call was not sent",
            ));
        }
        Ok(Self {
            augmented: support != TaskSupport::Forbidden && capabilities.tool_call,
            capabilities,
            generation,
        })
    }

    fn check(
        &self,
        manager: &McpManager,
        owner: &AgentCx,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
    ) -> Result<()> {
        catalog::check_request_owner(owner)?;
        manager.check_running()?;
        manager.check_trust(entry)?;
        if !is_current(entry, transport) || transport.progress_generation() != self.generation {
            return Err(task_generation_changed());
        }
        if !transport.is_alive() {
            return Err(tool_err(
                "MCP_TRANSPORT_CLOSED",
                "connection closed while awaiting an MCP task; the originating tool call was not replayed",
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub(super) async fn run(
        self,
        manager: &McpManager,
        owner: &AgentCx,
        entry: &Arc<ServerEntry>,
        transport: &Arc<dyn McpTransport>,
        mut params: Value,
        timeout: Duration,
        dispatch: Arc<RequestDispatch>,
        on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
        elicitation: Option<crate::mcp::elicitation::TaskBinding>,
    ) -> Result<Value> {
        if !self.augmented {
            return transport
                .request_in_task_generation("tools/call", params, timeout, dispatch, self.generation)
                .await;
        }
        self.check(manager, owner, entry, transport)?;
        let deadline = Instant::now().checked_add(timeout).ok_or_else(task_timeout)?;
        // Retention is a request to the server, not a new execution deadline.
        // The receiver may choose another TTL; our original timeout still wins.
        params["task"] = json!({"ttl": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)});
        let receipt = transport
            .request_in_task_generation(
                "tools/call",
                params,
                remaining(deadline)?,
                dispatch,
                self.generation,
            )
            .await?;
        // Arm cleanup as soon as the server gives us a valid task identity,
        // before checking the remainder of its potentially malformed receipt.
        let mut active = ActiveTask {
            transport: Arc::clone(transport),
            id: Task::created_id(&receipt).map_err(|_| {
                tool_err(
                    "MCP_DELIVERY_INDETERMINATE",
                    "task creation returned no valid task identity; remote effects may already have occurred and the call was not replayed",
                )
            })?,
            generation: self.generation,
            can_cancel: self.capabilities.cancel,
            settled: false,
        };
        self.check(manager, owner, entry, transport)?;
        let mut task = Task::from_create_result(&receipt)?;
        if let Some(elicitation) = elicitation {
            elicitation.bind(&active.id);
        }
        let mut updates = StatusUpdates::default();
        loop {
            self.check(manager, owner, entry, transport)?;
            remaining(deadline)?;
            updates.emit(&task, on_update);
            // A callback can revoke trust, cancel the owner, or replace a
            // connection. Do not issue another request after that boundary.
            self.check(manager, owner, entry, transport)?;
            if task.status.is_terminal() || task.status == TaskStatus::InputRequired {
                // input_required asks us to open tasks/result now so the
                // server can deliver its supported client requests there.
                let result = transport
                    .request_in_task_generation(
                        "tasks/result",
                        json!({"taskId": active.id}),
                        remaining(deadline)?,
                        Arc::new(RequestDispatch::new()),
                        self.generation,
                    )
                    .await;
                self.check(manager, owner, entry, transport)?;
                remaining(deadline)?;
                match result {
                    Ok(value) => {
                        task.validate_result(&value)?;
                        active.settled = true;
                        return Ok(value);
                    }
                    Err(error) => {
                        // A terminal task needs no cancellation even when
                        // its underlying operation returned a JSON-RPC error.
                        // Transport loss/input-required failures remain owned.
                        if task.status.is_terminal() && is_server_error(&error) {
                            active.settled = true;
                        }
                        return Err(error);
                    }
                }
            }

            // Respect the server's polling interval without extending the
            // manager's single deadline. The outer owner boundary interrupts
            // this sleep immediately on cancellation or expiry.
            let wait = task.poll_interval.min(remaining(deadline)?);
            let now = owner
                .cx()
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |timer| timer.now());
            asupersync::time::sleep(now, wait).await;
            self.check(manager, owner, entry, transport)?;
            let status = transport
                .request_in_task_generation(
                    "tasks/get",
                    json!({"taskId": active.id}),
                    remaining(deadline)?,
                    Arc::new(RequestDispatch::new()),
                    self.generation,
                )
                .await?;
            self.check(manager, owner, entry, transport)?;
            task = Task::from_status_result(&status, &task)?;
        }
    }
}

fn task_timeout() -> Error {
    tool_err(
        "MCP_TIMEOUT",
        "MCP task exhausted its original tool-call deadline",
    )
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(task_timeout())
    } else {
        Ok(remaining)
    }
}

fn task_generation_changed() -> Error {
    tool_err(
        "MCP_DELIVERY_INDETERMINATE",
        "protocol session changed during task execution; remote effects may already have occurred and the task was not replayed",
    )
}

fn is_server_error(error: &Error) -> bool {
    matches!(error, Error::Tool { tool, message }
        if tool == "mcp" && message.starts_with("[MCP_SERVER_ERROR] "))
}

/// Cancellation uses the exact task and protocol generation admitted above.
/// It is best effort, never an assertion that the server stopped its effects.
struct ActiveTask {
    transport: Arc<dyn McpTransport>,
    id: String,
    generation: u64,
    can_cancel: bool,
    settled: bool,
}

impl Drop for ActiveTask {
    fn drop(&mut self) {
        if !self.settled && self.can_cancel {
            self.transport.cancel_task(&self.id, self.generation);
        }
    }
}

#[derive(Default)]
struct StatusUpdates {
    last: Option<(TaskStatus, Option<String>)>,
    delivered: usize,
}

impl StatusUpdates {
    fn emit(&mut self, task: &Task, on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>) {
        let Some(on_update) = on_update else {
            return;
        };
        if self.delivered >= MAX_STATUS_UPDATES
            || self.last.as_ref().is_some_and(|(status, message)| {
                *status == task.status && *message == task.status_message
            })
        {
            return;
        }
        self.last = Some((task.status, task.status_message.clone()));
        self.delivered += 1;
        on_update(ToolUpdate {
            content: vec![ContentBlock::Text(TextContent::new(task.progress_text()))],
            details: Some(json!({"mcpTask": {
                "status": task.status.as_str(),
                "statusMessage": task.status_message,
                "statusMessageTruncated": task.status_message_truncated,
                "deliveryLimited": self.delivered == MAX_STATUS_UPDATES,
            }})),
        });
    }
}
