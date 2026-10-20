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

#[cfg(not(unix))]
pub(in crate::subagents) async fn run(
    _runner: ChildRunner,
    _agent: AgentDefinition,
    state: Shared,
) -> Result<ToolOutput> {
    let reason =
        "PI_SUBAGENT_WORKER_UNSUPPORTED: warm workers require Unix nonblocking process pipes";
    crate::subagents::worker::finish(&state, Some(reason.to_string()));
    Err(Error::tool("subagent", reason))
}

#[cfg(unix)]
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

#[cfg(unix)]
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
    std::io::{self, Write},
    std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    std::time::Duration,
};

#[cfg(unix)]
const CANCELLED: &str =
    "PI_SUBAGENT_WORKER_CANCELLED: worker stopped before completion; prior side effects may remain";
#[cfg(unix)]
const INVALID_RPC: &str = "PI_SUBAGENT_WORKER_PROTOCOL: unexpected or malformed RPC frame";
#[cfg(unix)]
const COMMAND_LIMIT: usize = 512 * 1024;
#[cfg(unix)]
const STDERR_LIMIT: usize = 16 * 1024;
#[cfg(unix)]
const POLL_INTERVAL: Duration = Duration::from_millis(20);

#[cfg(unix)]
struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
}

#[cfg(unix)]
struct Actor {
    runner: ChildRunner,
    agent: AgentDefinition,
    state: Shared,
    owner: AgentCx,
    hub: HubLease,
    lifecycle: SubagentResult,
    child: Option<ChildProcessGuard>,
    stdin: Option<ChildStdin>,
    stdout: Option<FramedPipe<ChildStdout>>,
    stderr: Option<FramedPipe<ChildStderr>>,
    diagnostic: String,
    pending: Option<PendingWrite>,
    request_sequence: u64,
    failure: Option<String>,
}

#[cfg(unix)]
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

    fn spawn(&mut self) -> Result<()> {
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
        let entry = self
            .hub
            .register_worker(&self.agent.name, self.runner.hub_kind)?;
        self.lifecycle.hub_id = Some(entry.id.clone());
        self.check()?;
        let channel = if self.lifecycle.tools.iter().any(|tool| tool == "hub") {
            Some(crate::agent_hub::ipc::ChildChannel::bind(
                &entry.id,
                &self.owner,
                self.runner.deadline.remaining(),
            )?)
        } else {
            None
        };
        let args = crate::subagents::worker_args(
            &self.agent,
            self.runner.role_model_spec.as_deref(),
            self.runner.parent_model.as_ref(),
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
        use std::os::unix::process::CommandExt as _;
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
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        self.lifecycle.pid = Some(child.id());
        if !self.hub.mark_running(child.id(), &mut self.lifecycle) {
            return Err(Error::tool("subagent", CANCELLED));
        }
        let process = child
            .child
            .as_mut()
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        let flags =
            rustix::fs::fcntl_getfl(&stdin).map_err(|_| Error::tool("subagent", INVALID_RPC))?;
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
            match stdin.write(&pending.bytes[pending.offset..end]) {
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
        let mut output = result_output(vec![result], "worker", false);
        if let Some(details) = output.details.as_mut() {
            details["sessionIsolation"] = json!("persistent_rpc_worker");
            details["turnId"] = json!(pending.id);
        }
        crate::subagents::worker::complete_turn(&self.state, pending.id, Ok(output));
        Ok(())
    }

    async fn execute(&mut self) -> Result<ToolOutput> {
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

#[cfg(unix)]
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
        crate::subagents::worker::finish(&self.state, self.failure.take());
    }
}

#[cfg(unix)]
fn task(agent: &AgentDefinition, text: String) -> SubagentTask {
    SubagentTask {
        agent: agent.name.clone(),
        task: text,
        cwd: None,
        isolation: None,
        iso_apply: None,
        output_schema: None,
        schema_mode: SchemaMode::Permissive,
    }
}

#[cfg(unix)]
fn frame(line: &str) -> Result<Value> {
    let event: Value =
        serde_json::from_str(line).map_err(|_| Error::tool("subagent", INVALID_RPC))?;
    if event.get("type").and_then(Value::as_str).is_none() {
        return Err(Error::tool("subagent", INVALID_RPC));
    }
    Ok(event)
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
struct IdleReply {
    id: String,
    idle: Option<bool>,
}

#[cfg(unix)]
impl IdleReply {
    const fn new(id: String) -> Self {
        Self { id, idle: None }
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
        self.idle = Some(!streaming && !compacting);
        Ok(())
    }
}

#[cfg(unix)]
struct TurnReply {
    id: String,
    acknowledged: bool,
    started: bool,
    complete: bool,
    protocol: protocol::ChildProtocol,
}

#[cfg(unix)]
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
            "data":{"isStreaming":false, "isCompacting":false}});
        assert!(
            IdleReply::new("new".to_string())
                .ingest(&wrong.to_string(), true)
                .is_err()
        ); // ubs:ignore[rust.panic.assert-macros] -- A prior readiness response cannot authorize the new barrier.
        assert!(idle_event(&ended("unexpected background answer")).is_err()); // ubs:ignore[rust.panic.assert-macros] -- An idle worker cannot accept unsolicited provider activity.
    }
}
