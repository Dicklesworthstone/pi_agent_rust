//! One real RPC process per retained worker. Prompt acknowledgments and
//! validated terminal answers are separate gates; a correlated idle barrier
//! also waits for RPC's post-turn persistence and compaction before reuse.
//!
//! The actor owns every pipe and its process lease. A cancelled carrier drops
//! that ownership before publishing stopped state, including when idle or
//! blocked by a child that no longer consumes stdin.

use super::ChildRunner;
use crate::error::{Error, Result};
use crate::subagents::AgentDefinition;
use crate::subagents::worker::Shared;
use crate::tools::ToolOutput;

#[cfg(not(any(unix, windows)))]
pub(in crate::subagents) async fn run(
    _runner: ChildRunner,
    _agent: AgentDefinition,
    state: Shared,
) -> Result<ToolOutput> {
    let reason = "PI_SUBAGENT_WORKER_UNSUPPORTED: warm workers require Unix or Windows nonblocking process pipes";
    crate::subagents::worker::finish(&state, Some(reason));
    Err(Error::tool("subagent", reason))
}

#[cfg(any(unix, windows))]
pub(in crate::subagents) fn run(
    runner: ChildRunner,
    agent: AgentDefinition,
    state: Shared,
) -> impl Future<Output = Result<ToolOutput>> + Send {
    // Construct the guard before returning a future. Even unpolled work has
    // an owner to settle, and its Drop implementation orders actual process
    // retirement before the facade's terminal publication.
    let mut actor = Actor::new(runner, agent, state);
    async move {
        let outcome = actor.execute().await;
        actor.failure = outcome.as_ref().err().map(ToString::to_string);
        if let Err(error) = &outcome {
            actor.lifecycle.fail(error.to_string());
        } else {
            actor.lifecycle.status = crate::subagents::SubagentStatus::Completed;
            actor.lifecycle.output = "Persistent worker closed.".to_string();
        }
        drop(actor);
        outcome
    }
}

#[cfg(any(unix, windows))]
use {
    super::ownership::HubLease,
    super::pipes::FramedPipe,
    super::{ChildProcessGuard, check_budget},
    crate::agent_cx::AgentCx,
    crate::model::{ContentBlock, TextContent},
    crate::subagents::{
        SchemaMode, SubagentResult, SubagentStatus, SubagentTask, protocol, result_output,
    },
    serde_json::{Value, json},
    std::future::Future,
    std::io,
    std::process::{Command, Stdio},
    std::time::Duration,
};

#[cfg(unix)]
use std::io::Write as _;

#[cfg(unix)]
type WorkerStdin = std::process::ChildStdin;
#[cfg(unix)]
type WorkerStdout = std::process::ChildStdout;
#[cfg(unix)]
type WorkerStderr = std::process::ChildStderr;
#[cfg(windows)]
type WorkerStdin = crate::jobs::windows_job_pipe::Writer;
#[cfg(windows)]
type WorkerStdout = super::pipes::WindowsReader;
#[cfg(windows)]
type WorkerStderr = super::pipes::WindowsReader;

#[cfg(any(unix, windows))]
const CANCELLED: &str =
    "PI_SUBAGENT_WORKER_CANCELLED: worker stopped before completion; prior side effects may remain";
#[cfg(any(unix, windows))]
const INVALID_RPC: &str = "PI_SUBAGENT_WORKER_PROTOCOL: unexpected or malformed RPC frame";
#[cfg(any(unix, windows))]
const COMMAND_LIMIT: usize = 512 * 1024;
#[cfg(any(unix, windows))]
const STDERR_LIMIT: usize = 16 * 1024;
#[cfg(any(unix, windows))]
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[cfg(any(unix, windows))]
struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
}

#[cfg(any(unix, windows))]
struct Actor {
    runner: ChildRunner,
    agent: AgentDefinition,
    state: Shared,
    owner: AgentCx,
    hub: HubLease,
    lifecycle: SubagentResult,
    child: Option<ChildProcessGuard>,
    stdin: Option<WorkerStdin>,
    stdout: Option<FramedPipe<WorkerStdout>>,
    stderr: Option<FramedPipe<WorkerStderr>>,
    diagnostic: String,
    pending: Option<PendingWrite>,
    request_sequence: u64,
    failure: Option<String>,
    output_contract: Option<crate::subagents::worker::output_contract::OutputContract>,
}

#[cfg(any(unix, windows))]
impl Actor {
    fn new(runner: ChildRunner, agent: AgentDefinition, state: Shared) -> Self {
        let lifecycle = SubagentResult::starting(
            &agent,
            task(&agent, "Persistent RPC worker".to_string()),
            None,
            &runner.child_binary,
            &runner.cwd,
            &[],
        );
        Self {
            runner,
            agent,
            state,
            owner: AgentCx::for_current_or_request(),
            hub: HubLease::empty(),
            lifecycle,
            child: None,
            stdin: None,
            stdout: None,
            stderr: None,
            diagnostic: String::new(),
            pending: None,
            request_sequence: 0,
            failure: Some(CANCELLED.to_string()),
            output_contract: None,
        }
    }

    fn check(&mut self) -> Result<()> {
        if !check_budget(&self.owner, self.runner.deadline, &mut self.lifecycle) {
            return Err(Error::tool(
                "subagent",
                self.lifecycle.error.as_deref().unwrap_or(CANCELLED),
            ));
        }
        if crate::subagents::worker::should_close(&self.state) {
            return Err(Error::tool("subagent", CANCELLED));
        }
        if let Some(child) = &mut self.child {
            child.poll_hub();
            if child
                .try_wait()
                .map_err(|_| {
                    Error::tool(
                        "subagent",
                        "PI_SUBAGENT_WORKER_PROCESS: failed to observe worker process",
                    )
                })?
                .is_some()
            {
                return Err(Error::tool(
                    "subagent",
                    "PI_SUBAGENT_WORKER_EXITED: persistent child exited unexpectedly",
                ));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // One ordered launch: hub entry, channel, command, lease.
    fn spawn(&mut self) -> Result<()> {
        #[cfg(unix)]
        use std::os::unix::process::CommandExt as _;
        self.check()?;
        let capabilities = self.owner.capabilities();
        if !capabilities.io || !capabilities.spawn || !capabilities.time {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_PERMISSION: warm workers require I/O, spawn and timer capabilities",
            ));
        }
        if !self.runner.cwd.is_dir() {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_WORKER_CWD: launch directory is unavailable",
            ));
        }
        if let Some(budget) = &self.runner.cost_budget {
            budget.admit()?;
        }
        let entry = self
            .hub
            .register_worker(&self.agent.name, self.runner.hub_kind)?;
        self.lifecycle.hub_id = Some(entry.id.clone());
        crate::subagents::worker::bind_hub(&self.state, &entry.id)?;
        self.check()?;
        let messaging = self.lifecycle.tools.iter().any(|tool| tool == "hub");
        let channel = if messaging || self.runner.cost_budget.is_some() {
            Some(
                crate::agent_hub::ipc::ChildChannel::bind(
                    &entry.id,
                    &self.owner,
                    self.runner.deadline.remaining(),
                )?
                .with_cost_budget(self.runner.cost_budget.clone(), messaging),
            )
        } else {
            None
        };
        let args = crate::subagents::with_fallback_policy_args(
            crate::subagents::worker_args(
                &self.agent,
                self.runner.role_model_spec.as_deref(),
                self.runner.parent_model.as_ref(),
                self.runner.worker_model_selection.as_ref(),
            ),
            self.runner.worker_fallback_policy.as_deref(),
        );
        let mut command = Command::new(&self.runner.child_binary);
        crate::agent_hub::ipc::ChildChannel::configure_child(&mut command, channel.as_ref());
        self.runner
            .deadline
            .configure_child(&mut command)
            .map_err(|error| Error::tool("subagent", error))?;
        command
            .args(args)
            .current_dir(&self.runner.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PI_CODING_AGENT_DIR", &self.runner.global_dir)
            .env("PI_SUBAGENT_PARENT_PID", std::process::id().to_string())
            .env(
                "PI_SUBAGENT_DEPTH",
                crate::subagents::child_depth().to_string(),
            )
            .env("PI_SUBAGENT_STEER_FILE", &entry.steer_path)
            .env("PI_SUBAGENT_RUN_ID", &entry.id);
        #[cfg(windows)]
        let (stdin, stdout, stderr) = {
            // All parent endpoints are nonblocking before the guarded spawn.
            // The child receives ordinary synchronous standard handles.
            let (stdin, child_stdin) = crate::jobs::windows_job_pipe::stdin_pair()
                .map_err(|_| Error::tool("subagent", INVALID_RPC))?;
            command.stdin(Stdio::from(child_stdin));
            let (stdout, stderr) = super::pipes::ChildPipes::configure(&mut command)
                .map_err(|_| Error::tool("subagent", INVALID_RPC))?
                .into_frames();
            (stdin, stdout, stderr)
        };
        #[cfg(unix)]
        command.process_group(0);
        self.check()?;
        self.child = Some(
            ChildProcessGuard::spawn(&self.owner, &mut command)
                .map_err(|_| {
                    Error::tool(
                        "subagent",
                        "PI_SUBAGENT_WORKER_SPAWN: failed to launch worker",
                    )
                })?
                .with_process_lifetime(self.hub.process_lifetime())
                .with_hub_channel(channel),
        );
        // On Windows Command retains duplicate child pipe handles until it
        // drops. Close them now so actor retirement owns the whole transport.
        drop(command);
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        self.lifecycle.pid = Some(child.id());
        if !self.hub.mark_running(child.id(), &mut self.lifecycle) {
            return Err(Error::tool("subagent", CANCELLED));
        }
        #[cfg(unix)]
        {
            let process = child
                .child
                .as_mut()
                .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
            let stdin = process
                .stdin
                .take()
                .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
            let flags = rustix::fs::fcntl_getfl(&stdin)
                .map_err(|_| Error::tool("subagent", INVALID_RPC))?;
            rustix::fs::fcntl_setfl(&stdin, flags | rustix::fs::OFlags::NONBLOCK)
                .map_err(|_| Error::tool("subagent", INVALID_RPC))?;
            self.stdin = Some(stdin);
            self.stdout = Some(
                FramedPipe::new(
                    process
                        .stdout
                        .take()
                        .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?,
                    protocol::MAX_FRAME_BYTES,
                )
                .map_err(|_| Error::tool("subagent", INVALID_RPC))?,
            );
            self.stderr = Some(
                FramedPipe::new(
                    process
                        .stderr
                        .take()
                        .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?,
                    protocol::MAX_FRAME_BYTES,
                )
                .map_err(|_| Error::tool("subagent", INVALID_RPC))?,
            );
        }
        #[cfg(windows)]
        {
            self.stdin = Some(stdin);
            self.stdout = Some(stdout);
            self.stderr = Some(stderr);
        }
        self.check()
    }

    fn queue(&mut self, command: &str, message: Option<&str>) -> Result<String> {
        if self.pending.is_some() {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        self.request_sequence = self
            .request_sequence
            .checked_add(1)
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        let id = format!("warm-{}", self.request_sequence);
        let mut request = json!({"id": id, "type": command});
        if let Some(message) = message {
            let message = self
                .output_contract
                .as_ref()
                .map_or_else(|| message.to_owned(), |contract| contract.prompt(message));
            request["message"] = json!(format!("Task: {message}"));
        }
        let mut bytes =
            serde_json::to_vec(&request).map_err(|_| Error::tool("subagent", INVALID_RPC))?;
        if bytes.len() >= COMMAND_LIMIT {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_WORKER_INPUT_LIMIT: encoded command exceeds 512 KiB",
            ));
        }
        bytes.push(b'\n');
        self.pending = Some(PendingWrite { bytes, offset: 0 });
        Ok(id)
    }

    fn write_slice(&mut self) -> Result<()> {
        let Some(pending) = &mut self.pending else {
            return Ok(());
        };
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        for _ in 0..4 {
            let end = pending.bytes.len().min(pending.offset.saturating_add(8192));
            #[cfg(unix)]
            let written = stdin.write(&pending.bytes[pending.offset..end]);
            #[cfg(windows)]
            let written = stdin.try_write(&pending.bytes[pending.offset..end]);
            match written {
                Ok(0) => {
                    return Err(Error::tool(
                        "subagent",
                        "PI_SUBAGENT_WORKER_PIPE: child stdin closed",
                    ));
                }
                Ok(written) => pending.offset += written,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    return Err(Error::tool(
                        "subagent",
                        "PI_SUBAGENT_WORKER_PIPE: failed to write child stdin",
                    ));
                }
            }
            if pending.offset == pending.bytes.len() {
                self.pending = None;
                break;
            }
        }
        Ok(())
    }

    /// Bound complete frames and partial-frame work independently on both
    /// pipes. Neither an endless line nor a blocked stdin can hide a carrier
    /// cancellation or the worker's immutable lifetime deadline.
    fn tick(&mut self, mut accept: impl FnMut(&str, bool) -> Result<()>) -> Result<()> {
        self.check()?;
        self.write_slice()?;
        let sent = self.pending.is_none();
        let mut stdout_budget = 32;
        let mut stderr_budget = 32;
        for _ in 0..16 {
            let stdout = self
                .stdout
                .as_mut()
                .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?
                .next_frame(&mut stdout_budget)
                .map_err(|error| Error::tool("subagent", error))?;
            let stderr = self
                .stderr
                .as_mut()
                .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?
                .next_frame(&mut stderr_budget)
                .map_err(|error| Error::tool("subagent", error))?;
            let progressed = stdout.is_some() || stderr.is_some();
            if let Some(bytes) = stdout {
                let line = String::from_utf8(bytes).map_err(|_| {
                    Error::tool(
                        "subagent",
                        "PI_SUBAGENT_WORKER_PROTOCOL: stdout is not UTF-8",
                    )
                })?;
                accept(&line, sent)?;
            }
            if let Some(bytes) = stderr {
                let line = String::from_utf8_lossy(&bytes);
                let remaining = STDERR_LIMIT.saturating_sub(self.diagnostic.len());
                let mut end = remaining.min(line.len());
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                self.diagnostic.push_str(&line[..end]);
                if self.diagnostic.len() < STDERR_LIMIT {
                    self.diagnostic.push('\n');
                }
            }
            if !progressed {
                break;
            }
        }
        if self.stdout.as_ref().is_some_and(FramedPipe::is_closed) {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_WORKER_PIPE: child stdout closed",
            ));
        }
        self.check()
    }

    async fn idle_barrier(&mut self) -> Result<()> {
        loop {
            let id = self.queue("get_state", None)?;
            let mut reply = IdleReply::new(id);
            loop {
                self.tick(|line, sent| reply.ingest(line, sent))?;
                if let Some(idle) = reply.idle {
                    if idle {
                        crate::subagents::worker::observe_idle_context(
                            &self.state,
                            reply.context_tokens,
                            reply.context_window,
                        );
                        return Ok(());
                    }
                    break;
                }
                self.owner.time().sleep(POLL_INTERVAL).await;
            }
            self.owner.time().sleep(POLL_INTERVAL).await;
        }
    }

    async fn turn(&mut self, pending: crate::subagents::worker::PendingTurn) -> Result<()> {
        self.diagnostic.clear();
        let id = self.queue("prompt", Some(&pending.task))?;
        let mut reply = TurnReply::new(id);
        let mut result = SubagentResult::starting(
            &self.agent,
            task(&self.agent, pending.task),
            None,
            &self.runner.child_binary,
            &self.runner.cwd,
            &[],
        );
        result.pid = self.lifecycle.pid;
        result.hub_id.clone_from(&self.lifecycle.hub_id);
        result.session_isolation = "persistent_rpc_worker";
        result.status = SubagentStatus::Running;
        let outcome = async {
            loop {
                self.tick(|line, sent| reply.ingest(line, sent, &mut result.output))?;
                if reply.complete {
                    break;
                }
                self.owner.time().sleep(POLL_INTERVAL).await;
            }
            self.idle_barrier().await?;
            self.check()?;
            Ok::<(), Error>(())
        }
        .await;
        // Failed turns remain owned by the actor until Drop reaps the child.
        // The facade's finish path then settles the active and queued turns.
        outcome?;
        result.status = SubagentStatus::Completed;
        result.stderr.clone_from(&self.diagnostic);
        if let Some(contract) = &self.output_contract {
            // Only a complete, fresh answer after post-turn persistence and
            // compaction reaches validation. Failure settles this turn without
            // repeating its effects or discarding the reusable conversation.
            contract.validate(&mut result);
        }
        let mut output = result_output(&[result], "worker", false);
        if let Some(details) = output.details.as_mut() {
            details["sessionIsolation"] = json!("persistent_rpc_worker");
            details["turnId"] = json!(pending.id);
        }
        crate::subagents::worker::complete_turn(&self.state, pending.id, Ok(output));
        Ok(())
    }

    async fn execute(&mut self) -> Result<ToolOutput> {
        self.output_contract = crate::subagents::worker::output_contract::OutputContract::prepare(
            self.agent.output_schema.as_ref(),
        )?;
        self.spawn()?;
        self.idle_barrier().await?;
        loop {
            if crate::subagents::worker::should_close(&self.state) {
                break;
            }
            if let Some(turn) = crate::subagents::worker::next_turn(&self.state) {
                self.turn(turn).await?;
            } else {
                self.tick(|line, _| idle_event(line))?;
                self.owner.time().sleep(POLL_INTERVAL).await;
            }
        }
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "Persistent worker closed.",
            ))],
            details: Some(json!({"schema":"pi.subagent.worker.closed.v1"})),
            is_error: false,
        })
    }
}

#[cfg(any(unix, windows))]
impl Drop for Actor {
    fn drop(&mut self) {
        // Descriptor retirement and process-tree reaping precede every
        // stopped-state publication, including panic and future cancellation.
        drop(self.stdin.take());
        drop(self.stdout.take());
        drop(self.stderr.take());
        drop(self.child.take());
        if self.failure.is_some() && !self.lifecycle.is_error {
            self.lifecycle.status = SubagentStatus::Cancelled;
            self.lifecycle.is_error = true;
            self.lifecycle.error.clone_from(&self.failure);
        }
        self.hub.settle(&self.lifecycle);
        crate::subagents::worker::finish(&self.state, self.failure.take().as_deref());
    }
}

#[cfg(any(unix, windows))]
fn task(agent: &AgentDefinition, text: String) -> SubagentTask {
    SubagentTask {
        agent: agent.name.clone(),
        task: text,
        task_file: None,
        task_file_sha256: None,
        model: None,
        model_selection: None,
        fallback_policy: None,
        cwd: None,
        isolation: None,
        iso_apply: None,
        output_schema: None,
        schema_mode: SchemaMode::Permissive,
    }
}

#[cfg(any(unix, windows))]
fn frame(line: &str) -> Result<Value> {
    let event: Value =
        serde_json::from_str(line).map_err(|_| Error::tool("subagent", INVALID_RPC))?;
    if event.get("type").and_then(Value::as_str).is_none() {
        return Err(Error::tool("subagent", INVALID_RPC));
    }
    Ok(event)
}

#[cfg(any(unix, windows))]
fn turn_event(kind: &str) -> bool {
    matches!(
        kind,
        "agent_start"
            | "agent_end"
            | "turn_start"
            | "turn_end"
            | "message_start"
            | "message_update"
            | "message_end"
            | "tool_execution_start"
            | "tool_execution_update"
            | "tool_execution_end"
    )
}

#[cfg(any(unix, windows))]
fn idle_event(line: &str) -> Result<()> {
    let event = frame(line)?;
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "error" || kind == "response" || turn_event(kind) {
        return Err(Error::tool("subagent", INVALID_RPC));
    }
    Ok(())
}

#[cfg(any(unix, windows))]
struct IdleReply {
    id: String,
    idle: Option<bool>,
    context_tokens: Option<u64>,
    context_window: Option<u64>,
}

#[cfg(any(unix, windows))]
impl IdleReply {
    const fn new(id: String) -> Self {
        Self {
            id,
            idle: None,
            context_tokens: None,
            context_window: None,
        }
    }

    fn ingest(&mut self, line: &str, sent: bool) -> Result<()> {
        let event = frame(line)?;
        if event.get("type").and_then(Value::as_str) != Some("response") {
            return idle_event(line);
        }
        if !sent
            || self.idle.is_some()
            || event.get("id").and_then(Value::as_str) != Some(self.id.as_str())
            || event.get("command").and_then(Value::as_str) != Some("get_state")
            || event.get("success").and_then(Value::as_bool) != Some(true)
        {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        let streaming = event
            .pointer("/data/isStreaming")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        let compacting = event
            .pointer("/data/isCompacting")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        self.context_tokens = context_metric(&event, "contextTokens", true)?;
        self.context_window = context_metric(&event, "contextWindow", false)?;
        self.idle = Some(!streaming && !compacting);
        Ok(())
    }
}

#[cfg(any(unix, windows))]
fn context_metric(event: &Value, field: &str, allow_zero: bool) -> Result<Option<u64>> {
    match event.get("data").and_then(|data| data.get(field)) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|value| allow_zero || *value > 0)
            .map(Some)
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC)),
    }
}

#[cfg(any(unix, windows))]
struct TurnReply {
    id: String,
    acknowledged: bool,
    started: bool,
    complete: bool,
    protocol: protocol::ChildProtocol,
}

#[cfg(any(unix, windows))]
impl TurnReply {
    fn new(id: String) -> Self {
        Self {
            id,
            acknowledged: false,
            started: false,
            complete: false,
            protocol: protocol::ChildProtocol::default(),
        }
    }

    fn ingest(&mut self, line: &str, sent: bool, output: &mut String) -> Result<()> {
        let event = frame(line)?;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind == "response" {
            if !sent
                || self.acknowledged
                || event.get("id").and_then(Value::as_str) != Some(self.id.as_str())
                || event.get("command").and_then(Value::as_str) != Some("prompt")
                || event.get("success").and_then(Value::as_bool) != Some(true)
            {
                return Err(Error::tool(
                    "subagent",
                    "PI_SUBAGENT_WORKER_PROMPT: child rejected or mismatched prompt acknowledgment",
                ));
            }
            self.acknowledged = true;
            return Ok(());
        }
        if turn_event(kind) && (!self.acknowledged || self.complete) {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        if kind == "agent_start" {
            self.started = true;
        }
        if kind == "agent_end" && !self.started {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        self.protocol
            .ingest(line, output)
            .map_err(|error| Error::tool("subagent", error))?;
        if kind == "agent_end" {
            self.protocol
                .finish()
                .map_err(|error| Error::tool("subagent", error))?;
            self.complete = true;
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::subagents::SubagentTool;
    use crate::tools::Tool as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Instant;

    struct Fixture {
        root: tempfile::TempDir,
        tool: SubagentTool,
        owner: String,
    }

    impl Fixture {
        fn new(script: &str) -> Self {
            let root = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The real process regression needs its isolated fixture directory.
            let global = root.path().join("global");
            std::fs::create_dir_all(global.join("agents")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Agent discovery must read an authored controlled definition.
            std::fs::write(global.join("agents/worker.md"), "---\nname: worker\ndescription: retained worker fixture\ntools: []\n---\nRemember the earlier assignments.\n").unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Fixture creation failure must stop the test rather than use ambient agents.
            let binary = root.path().join("child.sh");
            std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- This child implements the real JSONL transport under test.
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The native fixture must be executable.
            let owner = format!("warm-fixture-{}", uuid::Uuid::new_v4());
            let mut tool = SubagentTool::with_paths(root.path().to_path_buf(), global, binary)
                .with_timeout(Duration::from_secs(15));
            tool.bind_job_session_scope(crate::jobs::JobSessionScope::fixed(&owner));
            Self { root, tool, owner }
        }

        async fn call(&self, input: Value) -> Value {
            self.tool
                .execute("warm-fixture", input, None)
                .await
                .expect("native worker operation") // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- This regression requires successful real tool admission.
                .details
                .expect("worker details") // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Native operation details are the observed public contract.
        }

        async fn wait(&self, worker: &str, turn: u64) -> Value {
            self.call(json!({"operation":"worker_wait", "workerId":worker, "turnId":turn, "timeoutMs":5000})).await
        }

        async fn await_file(&self, name: &str) {
            let started = Instant::now();
            while !self.root.path().join(name).exists()
                && started.elapsed() < Duration::from_secs(5)
            {
                AgentCx::for_request().time().sleep(POLL_INTERVAL).await;
            }
            assert!(
                self.root.path().join(name).exists(),
                "child did not reach its controlled boundary"
            ); // ubs:ignore[rust.panic.assert-macros] -- A missing process boundary would make the lifecycle regression invalid.
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(runtime) = asupersync::runtime::RuntimeBuilder::current_thread().build() {
                let _ = runtime.block_on(crate::jobs::kill_session(&self.owner));
            }
        }
    }

    fn field(value: &Value, key: &str) -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .expect("native string field")
            .to_string() // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Missing worker identity is a public-contract failure.
    }

    fn native_result(value: &Value) -> &Value {
        value
            .pointer("/turn/result/details/results/0")
            .expect("completed native result") // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- A wait must produce an actual child result, not a fabricated handle-only success.
    }

    const RPC_CHILD: &str = r#"
printf '%s\n' "$$" >> launches
memory=empty
busy=0
while IFS= read -r line; do
    id=${line#*\"id\":\"}
    id=${id%%\"*}
    case "$line" in
        *'"type":"get_state"'*)
            if [ "$busy" -eq 1 ]; then
                printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":true}}\n' "$id"
                busy=0
            else
                printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false}}\n' "$id"
            fi
            ;;
        *'"type":"prompt"'*)
            if [ "$busy" -eq 1 ]; then
                printf '{"type":"response","id":"%s","command":"prompt","success":false}\n' "$id"
                continue
            fi
            printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
            printf '%s\n' '{"type":"agent_start"}'
            case "$line" in
                *'Task: remember silver'*)
                    printf ready > first-running
                    while [ ! -f release ]; do sleep 0.02; done
                    memory=silver
                    ;;
            esac
            printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"%s"}]}]}\n' "$memory"
            busy=1
            ;;
    esac
done
"#;

    #[test]
    fn queued_turns_reuse_one_process_and_wait_for_post_turn_compaction() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Exercise the actual actor and session jobs on their native runtime.
        runtime.block_on(async {
            let started = fixture.call(json!({"operation":"worker_start", "agent":"worker", "task":"remember silver"})).await;
            let worker = field(&started, "workerId");
            fixture.await_file("first-running").await;
            let queued = fixture.call(json!({"operation":"worker_send", "workerId":worker, "task":"recall the remembered word"})).await;
            assert_eq!(queued.pointer("/turn/status").and_then(Value::as_str), Some("queued")); // ubs:ignore[rust.panic.assert-macros] -- The second task must queue while the first real child turn is gated.
            std::fs::write(fixture.root.path().join("release"), b"continue").unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Release the real controlled child after proving FIFO admission.
            let first = fixture.wait(&worker, 1).await;
            let second = fixture.wait(&worker, 2).await;
            assert_eq!(native_result(&first).get("pid"), native_result(&second).get("pid")); // ubs:ignore[rust.panic.assert-macros] -- Genuine reuse must retain the same live native process.
            assert_eq!(native_result(&second).get("output").and_then(Value::as_str), Some("silver")); // ubs:ignore[rust.panic.assert-macros] -- The second answer depends on memory established by the first prompt.
            let launches = std::fs::read_to_string(fixture.root.path().join("launches")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Startup count is independent evidence against one-shot respawning.
            assert_eq!(launches.lines().count(), 1); // ubs:ignore[rust.panic.assert-macros] -- Two results must have required only one process launch.
            let closed = fixture.call(json!({"operation":"worker_close", "workerId":worker})).await;
            assert_eq!(closed.get("status").and_then(Value::as_str), Some("closed")); // ubs:ignore[rust.panic.assert-macros] -- Explicit close must settle the retained carrier after both turns.
        });
    }

    #[test]
    fn closing_an_idle_worker_reaps_its_retained_pid() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The native actor requires its real event loop.
        runtime.block_on(async {
            let started = fixture.call(json!({"operation":"worker_start", "agent":"worker", "task":"immediate answer"})).await;
            let worker = field(&started, "workerId");
            let settled = fixture.wait(&worker, 1).await;
            let pid = native_result(&settled).get("pid").and_then(Value::as_i64).and_then(|pid| i32::try_from(pid).ok()).and_then(rustix::process::Pid::from_raw).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- A completed real worker must publish its live native PID.
            assert!(rustix::process::test_kill_process(pid).is_ok()); // ubs:ignore[rust.panic.assert-macros] -- The process must remain alive after the turn to establish warm reuse.
            fixture.call(json!({"operation":"worker_close", "workerId":worker})).await;
            assert!(rustix::process::test_kill_process(pid).is_err()); // ubs:ignore[rust.panic.assert-macros] -- Close must not return before the retained child has been reaped.
        });
    }

    #[test]
    fn session_shutdown_reaps_a_worker_that_stops_reading_commands() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(
            r#"
IFS= read -r line
id=${line#*\"id\":\"}
id=${id%%\"*}
printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false}}\n' "$id"
printf '%s' "$$" > stopped-reading
exec sleep 30
"#,
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Drive blocked-pipe cancellation through the native carrier.
        runtime.block_on(async {
            let started = fixture.call(json!({"operation":"worker_start", "agent":"worker", "task":"x".repeat(64 * 1024)})).await;
            let worker = field(&started, "workerId");
            fixture.await_file("stopped-reading").await;
            AgentCx::for_request().time().sleep(Duration::from_millis(200)).await;
            let pid = std::fs::read_to_string(fixture.root.path().join("stopped-reading")).unwrap().parse::<i32>().ok().and_then(rustix::process::Pid::from_raw).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The controlled child records its own PID before refusing further stdin.
            crate::jobs::kill_session(&fixture.owner).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Session shutdown must cancel the carrier despite backpressure.
            assert!(rustix::process::test_kill_process(pid).is_err()); // ubs:ignore[rust.panic.assert-macros] -- A non-reading child must be reaped before session cleanup finishes.
            let observed = fixture.call(json!({"operation":"worker_status", "workerId":worker})).await;
            assert!(matches!(observed.pointer("/turn/status").and_then(Value::as_str), Some("failed" | "cancelled"))); // ubs:ignore[rust.panic.assert-macros] -- A cancelled partial write must never become a successful completed turn.
        });
    }

    fn acknowledged(id: &str) -> String {
        json!({"type":"response", "id":id, "command":"prompt", "success":true}).to_string()
    }

    fn ended(text: &str) -> String {
        json!({"type":"agent_end", "messages":[{
            "role":"assistant", "stopReason":"stop",
            "content":[{"type":"text", "text":text}]
        }]})
        .to_string()
    }

    #[test]
    fn acknowledged_prompt_requires_a_fresh_run_and_terminal_answer() {
        let mut reply = TurnReply::new("turn-2".to_string());
        let mut output = String::new();
        reply
            .ingest(&acknowledged("turn-2"), true, &mut output)
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The valid control acknowledgment must be admitted by this regression.
        assert!(!reply.complete); // ubs:ignore[rust.panic.assert-macros] -- Prompt acknowledgment must never stand in for completion.
        assert!(
            reply
                .ingest(&ended("stale prior answer"), true, &mut output)
                .is_err()
        ); // ubs:ignore[rust.panic.assert-macros] -- A terminal frame without a new run must not settle the new prompt.

        let mut reply = TurnReply::new("turn-2".to_string());
        reply
            .ingest(&acknowledged("turn-2"), true, &mut output)
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Begin a separately admitted valid turn.
        reply
            .ingest(r#"{"type":"agent_start"}"#, true, &mut output)
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The fresh native run is required test input.
        reply
            .ingest(&ended("new answer"), true, &mut output)
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- A successful fresh terminal answer must be accepted.
        assert!(reply.complete); // ubs:ignore[rust.panic.assert-macros] -- Verify the actual validated terminal boundary.
        assert_eq!(output, "new answer"); // ubs:ignore[rust.panic.assert-macros] -- A stale result must never be substituted for the new answer.
        assert!(
            reply
                .ingest(&ended("duplicate"), true, &mut output)
                .is_err()
        ); // ubs:ignore[rust.panic.assert-macros] -- Duplicate completion must be fatal rather than authorizing another turn.
    }

    #[test]
    fn wrong_or_early_acknowledgment_never_authorizes_rpc_work() {
        for (id, sent) in [("wrong-id", true), ("expected", false)] {
            let mut reply = TurnReply::new("expected".to_string());
            assert!(
                reply
                    .ingest(&acknowledged(id), sent, &mut String::new())
                    .is_err()
            ); // ubs:ignore[rust.panic.assert-macros] -- Correlation and fully written input are required admission gates.
        }
        let mut reply = TurnReply::new("expected".to_string());
        assert!(
            reply
                .ingest(r#"{"type":"agent_start"}"#, true, &mut String::new())
                .is_err()
        ); // ubs:ignore[rust.panic.assert-macros] -- Unacknowledged background activity must not be adopted as this request.
        let rejected =
            json!({"type":"response", "id":"expected", "command":"prompt", "success":false});
        assert!(
            reply
                .ingest(&rejected.to_string(), true, &mut String::new())
                .is_err()
        ); // ubs:ignore[rust.panic.assert-macros] -- Rejected RPC admission must not masquerade as a provider result.
    }

    #[test]
    fn idle_barrier_waits_for_streaming_and_compaction_and_rejects_unsolicited_turns() {
        for (streaming, compacting) in [(true, false), (false, true), (false, false)] {
            let mut reply = IdleReply::new("ready".to_string());
            let response = json!({"type":"response", "id":"ready", "command":"get_state", "success":true,
                "data":{"isStreaming":streaming, "isCompacting":compacting}});
            reply.ingest(&response.to_string(), true).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The explicit RPC phase response is valid controlled input.
            assert_eq!(reply.idle, Some(!streaming && !compacting)); // ubs:ignore[rust.panic.assert-macros] -- Compaction remains a reuse barrier after agent_end has arrived.
        }
        let wrong = json!({"type":"response", "id":"old", "command":"get_state", "success":true,
            "data":{"isStreaming":false,"isCompacting":false}});
        assert!(
            IdleReply::new("new".to_string())
                .ingest(&wrong.to_string(), true)
                .is_err()
        ); // ubs:ignore[rust.panic.assert-macros] -- A prior readiness response cannot authorize the new barrier.
        assert!(idle_event(&ended("unexpected background answer")).is_err()); // ubs:ignore[rust.panic.assert-macros] -- An idle worker cannot accept unsolicited provider activity.
    }

    #[test]
    fn idle_context_telemetry_is_exact_optional_and_rejects_malformed_counters() {
        let response = json!({"type":"response", "id":"ready", "command":"get_state", "success":true,
            "data":{"isStreaming":false,"isCompacting":false,"contextTokens":0,"contextWindow":128_000}});
        let mut reply = IdleReply::new("ready".to_string());
        reply
            .ingest(&response.to_string(), true)
            .expect("valid context telemetry");
        assert_eq!(reply.context_tokens, Some(0));
        assert_eq!(reply.context_window, Some(128_000));
        for field in ["contextTokens", "contextWindow"] {
            for value in [json!(-1), json!(1.5), json!("100"), json!(true), json!({})] {
                let mut invalid = response.clone();
                invalid["data"][field] = value;
                assert!(
                    IdleReply::new("ready".to_string())
                        .ingest(&invalid.to_string(), true)
                        .is_err()
                );
            }
        }
        let mut unknown = response;
        unknown["data"]["contextTokens"] = Value::Null;
        unknown["data"]["contextWindow"] = Value::Null;
        let mut reply = IdleReply::new("ready".to_string());
        reply
            .ingest(&unknown.to_string(), true)
            .expect("unknown context telemetry");
        assert_eq!(reply.context_tokens, None);
        assert_eq!(reply.context_window, None);
        unknown["data"]["contextWindow"] = json!(0);
        assert!(
            IdleReply::new("ready".to_string())
                .ingest(&unknown.to_string(), true)
                .is_err()
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::subagents::SubagentTool;
    use crate::tools::Tool as _;
    use std::time::Instant;

    fn run_isolated(case: &str) -> bool {
        const CHILD_CASE: &str = "PI_TEST_WINDOWS_WARM_WORKER_CASE";
        if std::env::var(CHILD_CASE).as_deref() == Ok(case) {
            return false;
        }
        // A regression that blocks a pipe destructor would also prevent an
        // in-process timeout or Fixture::drop from running. Keep that failure
        // inside a child whose complete process tree has an independent owner.
        let _guard = crate::jobs::process_test_guard();
        let mut command = Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "--exact",
                &format!("subagents::execution::worker::windows_tests::{case}"),
                "--nocapture",
            ])
            .env(CHILD_CASE, case)
            .stdin(Stdio::null());
        let mut child = ChildProcessGuard::spawn(&AgentCx::for_request(), &mut command)
            .expect("isolated Windows worker regression");
        drop(command);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().expect("observe worker regression") {
                assert!(
                    status.success(),
                    "Windows worker regression failed: {status}"
                );
                return true;
            }
            assert!(
                Instant::now() < deadline,
                "Windows worker regression stalled"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    struct Fixture {
        root: tempfile::TempDir,
        tool: SubagentTool,
        owner: String,
    }

    impl Fixture {
        fn new(script: &str) -> Self {
            let root = tempfile::tempdir().expect("worker fixture root");
            let global = root.path().join("global");
            std::fs::create_dir_all(global.join("agents")).expect("agent directory");
            std::fs::write(
                global.join("agents/worker.md"),
                "---\nname: worker\ndescription: Windows RPC fixture\ntools: []\n---\nRemember previous assignments.\n",
            )
            .expect("agent definition");
            // The real public launch path passes its normal worker arguments.
            // Keep the definition body on one line for Windows batch argv;
            // every RPC request still travels through the production actor.
            let binary = root.path().join("child.bat");
            std::fs::write(
                &binary,
                format!("@echo off\r\n{}\r\n", script.replace('\n', "\r\n")),
            )
            .expect("RPC child");
            let owner = format!("windows-worker-{}", uuid::Uuid::new_v4());
            let mut tool = SubagentTool::with_paths(root.path().to_path_buf(), global, binary)
                .with_timeout(Duration::from_secs(15));
            tool.bind_job_session_scope(crate::jobs::JobSessionScope::fixed(&owner));
            Self { root, tool, owner }
        }

        async fn call(&self, input: Value) -> Value {
            self.tool
                .execute("windows-worker-fixture", input, None)
                .await
                .expect("native retained operation")
                .details
                .expect("retained operation details")
        }

        async fn await_file(&self, name: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.root.path().join(name).exists() {
                assert!(Instant::now() < deadline, "child did not reach {name}");
                AgentCx::for_request().time().sleep(POLL_INTERVAL).await;
            }
        }

        fn release(&self) {
            std::fs::write(self.root.path().join("release"), b"continue")
                .expect("release first turn");
        }

        fn assert_two_turn_exchange(&self) {
            let requests = std::fs::read_to_string(self.root.path().join("requests.log"))
                .expect("recorded RPC requests");
            let frames: Vec<Value> = requests
                .lines()
                .map(|line| serde_json::from_str(line).expect("JSON RPC request"))
                .collect();
            let kinds: Vec<_> = frames
                .iter()
                .map(|frame| frame["type"].as_str().expect("request type"))
                .collect();
            assert_eq!(
                kinds,
                [
                    "get_state",
                    "prompt",
                    "get_state",
                    "get_state",
                    "prompt",
                    "get_state",
                    "get_state",
                ]
            );
            for (index, frame) in frames.iter().enumerate() {
                assert_eq!(frame["id"], format!("warm-{}", index + 1));
            }
            let launches =
                std::fs::read_to_string(self.root.path().join("launches")).expect("launch count");
            assert_eq!(launches.lines().count(), 1);
            assert!(self.root.path().join("compacting").exists());
        }

        fn live_child_pids(&self) -> Vec<u32> {
            let path = self.tool.child_binary.to_string_lossy();
            let mut system = sysinfo::System::new();
            system.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::All,
                true,
                sysinfo::ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
            );
            system
                .processes()
                .values()
                .filter(|process| {
                    process.parent() == Some(sysinfo::Pid::from_u32(std::process::id()))
                        && process
                            .cmd()
                            .iter()
                            .any(|argument| argument.to_string_lossy().contains(path.as_ref()))
                })
                .map(|process| process.pid().as_u32())
                .collect()
        }

        fn running_pid(&self) -> u32 {
            let pids = self.live_child_pids();
            assert_eq!(pids.len(), 1, "one live native batch child");
            pids[0]
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Assertion cleanup must also work if a broken actor gets stuck
            // dropping a backpressured pipe before reaching its process guard.
            // Only this fixture's direct child can match its unique batch path.
            for pid in self.live_child_pids() {
                crate::tools::kill_process_tree(Some(pid));
            }
            if let Ok(runtime) = asupersync::runtime::RuntimeBuilder::current_thread().build() {
                let _ = runtime.block_on(crate::jobs::kill_session(&self.owner));
            }
        }
    }

    fn field(value: &Value, key: &str) -> String {
        value[key].as_str().expect("native identity").to_string()
    }

    fn process_alive(pid: u32) -> bool {
        let pid = sysinfo::Pid::from_u32(pid);
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        system.process(pid).is_some()
    }

    async fn assert_reaped(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "retained child survived shutdown"
            );
            AgentCx::for_request().time().sleep(POLL_INTERVAL).await;
        }
    }

    // JSON-only stdout lets the strict production RPC parser reject every
    // unexpected frame. Read counters validate IDs and command kinds; the
    // second prompt can succeed only after both post-turn get_state replies.
    const RPC_CHILD: &str = r#"
setlocal EnableExtensions EnableDelayedExpansion
set "sequence=0"
set "memory="
>>launches echo started
:read
set "line="
set /p "line="
if not defined line exit /b 0
set /a sequence+=1 >nul
if !sequence! GTR 7 exit /b 20
>request.json echo(!line!
>>requests.log echo(!line!
"%SystemRoot%\System32\findstr.exe" /L /C:"warm-!sequence!" request.json >nul || exit /b 21
if !sequence! EQU 2 goto remember
if !sequence! EQU 5 goto recall
"%SystemRoot%\System32\findstr.exe" /L /C:"get_state" request.json >nul || exit /b 22
set "compacting=false"
if !sequence! EQU 3 set "compacting=true"
if !sequence! EQU 6 set "compacting=true"
if "!compacting!"=="true" >compacting echo busy
echo {"type":"response","id":"warm-!sequence!","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":!compacting!,"contextTokens":64,"contextWindow":1000}}
goto read
:remember
"%SystemRoot%\System32\findstr.exe" /L /C:"prompt" request.json >nul || exit /b 23
"%SystemRoot%\System32\findstr.exe" /L /C:"remember silver" request.json >nul || exit /b 24
set "memory=silver"
echo {"type":"response","id":"warm-!sequence!","command":"prompt","success":true}
echo {"type":"agent_start"}
>first-running echo ready
:wait_release
if exist release goto answer
"%SystemRoot%\System32\ping.exe" -n 2 127.0.0.1 >nul
goto wait_release
:recall
"%SystemRoot%\System32\findstr.exe" /L /C:"prompt" request.json >nul || exit /b 25
"%SystemRoot%\System32\findstr.exe" /L /C:"recall" request.json >nul || exit /b 26
echo {"type":"response","id":"warm-!sequence!","command":"prompt","success":true}
echo {"type":"agent_start"}
:answer
echo {"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"!memory!"}]}]}
goto read
"#;

    #[test]
    fn windows_queued_turns_retain_context_and_close_the_idle_process() {
        if run_isolated("windows_queued_turns_retain_context_and_close_the_idle_process") {
            return;
        }
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let started = fixture
                .call(json!({"operation":"worker_start","agent":"worker","task":"remember silver"}))
                .await;
            let worker = field(&started, "workerId");
            fixture.await_file("first-running").await;
            let queued = fixture
                .call(json!({"operation":"worker_send","workerId":worker,"task":"recall the remembered word"}))
                .await;
            assert_eq!(queued.pointer("/turn/status"), Some(&json!("queued")));
            fixture.release();
            let first = fixture
                .call(json!({"operation":"worker_wait","workerId":worker,"turnId":1,"timeoutMs":5000}))
                .await;
            let second = fixture
                .call(json!({"operation":"worker_wait","workerId":worker,"turnId":2,"timeoutMs":5000}))
                .await;
            assert_eq!(first.pointer("/turn/status"), Some(&json!("completed")));
            assert_eq!(second.pointer("/turn/status"), Some(&json!("completed")));
            let first = first.pointer("/turn/result/details/results/0").expect("first result");
            let second_result = second.pointer("/turn/result/details/results/0").expect("second result");
            assert_eq!(first["pid"], second_result["pid"]);
            assert_eq!(second_result["output"], "silver");
            assert_eq!(second["contextTokens"], 64);
            assert_eq!(second["contextWindow"], 1000);
            let pid = u32::try_from(second_result["pid"].as_u64().expect("child PID"))
                .expect("Windows PID");
            assert!(process_alive(pid), "a completed turn must leave its worker alive");
            fixture.assert_two_turn_exchange();
            let closed = fixture
                .call(json!({"operation":"worker_close","workerId":worker}))
                .await;
            assert_eq!(closed["status"], "closed");
            assert_reaped(pid).await;
        });
    }

    #[test]
    fn windows_pool_drains_reused_members_and_reaps_them_after_finish() {
        if run_isolated("windows_pool_drains_reused_members_and_reaps_them_after_finish") {
            return;
        }
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let started = fixture
                .call(json!({"operation":"pool_start","agent":"worker","concurrency":1,"tasks":["remember silver","recall the remembered word"]}))
                .await;
            let pool = field(&started, "poolId");
            fixture.await_file("first-running").await;
            fixture.release();
            let first = fixture
                .call(json!({"operation":"pool_wait","poolId":pool,"itemId":1,"timeoutMs":5000}))
                .await;
            let second = fixture
                .call(json!({"operation":"pool_wait","poolId":pool,"itemId":2,"timeoutMs":5000}))
                .await;
            assert_eq!(first.pointer("/item/status"), Some(&json!("completed")));
            assert_eq!(second.pointer("/item/status"), Some(&json!("completed")));
            let first = first.pointer("/item/result/details/results/0").expect("first item");
            let second_result = second.pointer("/item/result/details/results/0").expect("second item");
            assert_eq!(first["pid"], second_result["pid"]);
            assert_eq!(second_result["output"], "silver");
            assert_eq!(second["status"], "open");
            assert_eq!(second.pointer("/workerContexts/0/tokens"), Some(&json!(64)));
            let pid = u32::try_from(second_result["pid"].as_u64().expect("member PID"))
                .expect("Windows PID");
            assert!(process_alive(pid), "an open pool must retain its idle member");
            fixture.assert_two_turn_exchange();
            fixture.call(json!({"operation":"pool_finish","poolId":pool})).await;
            let settled = fixture
                .call(json!({"operation":"pool_wait","poolId":pool,"timeoutMs":5000}))
                .await;
            assert_eq!(settled["status"], "completed");
            assert_eq!(settled["completedItems"], 2);
            assert_eq!(settled["sealed"], true);
            assert_reaped(pid).await;
        });
    }

    const STOP_READING_CHILD: &str = r#"
setlocal EnableExtensions EnableDelayedExpansion
set "line="
set /p "line="
if not defined line exit /b 0
>request.json echo(!line!
"%SystemRoot%\System32\findstr.exe" /L /C:"warm-1" request.json >nul || exit /b 21
"%SystemRoot%\System32\findstr.exe" /L /C:"get_state" request.json >nul || exit /b 22
echo {"type":"response","id":"warm-1","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false}}
>stopped-reading echo ready
:blocked
"%SystemRoot%\System32\ping.exe" -n 2 127.0.0.1 >nul
goto blocked
"#;

    #[test]
    fn windows_close_and_session_shutdown_cancel_backpressured_worker_input() {
        if run_isolated("windows_close_and_session_shutdown_cancel_backpressured_worker_input") {
            return;
        }
        let _guard = crate::jobs::process_test_guard();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        for session_shutdown in [false, true] {
            let fixture = Fixture::new(STOP_READING_CHILD);
            runtime.block_on(async {
                // Escaping doubles this valid 64 KiB task on the wire, beyond
                // the 64 KiB pipe buffer plus Mio's one accepted 8 KiB chunk.
                // The child reads only the small initial get_state command.
                let started = fixture
                    .call(json!({"operation":"worker_start","agent":"worker","task":"\"".repeat(64 * 1024)}))
                    .await;
                let worker = field(&started, "workerId");
                fixture.await_file("stopped-reading").await;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let state = fixture
                        .call(json!({"operation":"worker_status","workerId":worker}))
                        .await;
                    if state.pointer("/turn/status") == Some(&json!("running")) {
                        break;
                    }
                    assert!(Instant::now() < deadline, "the actor did not begin its prompt");
                    AgentCx::for_request().time().sleep(POLL_INTERVAL).await;
                }
                AgentCx::for_request().time().sleep(Duration::from_millis(200)).await;
                let pid = fixture.running_pid();
                let started = Instant::now();
                if session_shutdown {
                    crate::jobs::kill_session(&fixture.owner).await.expect("session shutdown");
                } else {
                    fixture.call(json!({"operation":"worker_close","workerId":worker})).await;
                }
                assert!(started.elapsed() < Duration::from_secs(5));
                assert_reaped(pid).await;
                let observed = fixture
                    .call(json!({"operation":"worker_status","workerId":worker}))
                    .await;
                assert!(matches!(observed["status"].as_str(), Some("closed" | "failed")));
                assert!(matches!(
                    observed.pointer("/turn/status").and_then(Value::as_str),
                    Some("cancelled" | "failed")
                ));
            });
        }
    }
}
