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
    crate::subagents::worker::output_contract::{BatchAnswer, OutputContract},
    crate::subagents::{
        SchemaMode, SubagentResult, SubagentStatus, SubagentTask, protocol, result_output,
    },
    serde_json::{Value, json},
    std::collections::{BTreeMap, BTreeSet},
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
    prompt_snapshot: Option<tempfile::NamedTempFile>,
    workspace_transport: Option<crate::subagents::workspace::PreparedRoots>,
    stdin: Option<WorkerStdin>,
    stdout: Option<FramedPipe<WorkerStdout>>,
    stderr: Option<FramedPipe<WorkerStderr>>,
    diagnostic: String,
    pending: Option<PendingWrite>,
    request_sequence: u64,
    failure: Option<String>,
    output_contract: Option<crate::subagents::worker::output_contract::OutputContract>,
    isolation: Option<crate::worktree_iso::IsoHandle>,
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
            prompt_snapshot: None,
            workspace_transport: None,
            stdin: None,
            stdout: None,
            stderr: None,
            diagnostic: String::new(),
            pending: None,
            request_sequence: 0,
            failure: Some(CANCELLED.to_string()),
            output_contract: None,
            isolation: None,
        }
    }

    fn check(&mut self) -> Result<()> {
        if !check_budget(&self.owner, self.runner.deadline, &mut self.lifecycle) {
            return Err(Error::tool(
                "subagent",
                self.lifecycle.error.as_deref().unwrap_or(CANCELLED),
            ));
        }
        if let Some(transport) = &self.workspace_transport {
            transport.lease.check()?;
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
        // Isolated launch depends on its frozen snapshot, not continued
        // existence of the parent's original directory while it was queued.
        if self.runner.worker_isolation.is_none() && !self.runner.cwd.is_dir() {
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
        if let Some(capture) = self.runner.worker_isolation.take() {
            // Materialize the pre-admission snapshot, never recapture a later
            // parent state. Keep ownership and recovery identity even if the
            // requested nested directory is absent or process startup fails.
            let handle = capture.materialize(&entry.id)?;
            self.isolation = Some(handle);
            let handle = self
                .isolation
                .as_ref()
                .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
            let descriptor = capture.descriptor(handle)?;
            crate::subagents::worker::bind_workspace(&self.state, descriptor)?;
            self.runner.cwd = capture.working_directory(handle)?;
            self.lifecycle.cwd.clone_from(&self.runner.cwd);
            self.check()?;
        }
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
        let mut args = crate::subagents::with_fallback_policy_args(
            crate::subagents::worker_args(
                &self.agent,
                self.runner.role_model_spec.as_deref(),
                self.runner.parent_model.as_ref(),
                self.runner.worker_model_selection.as_ref(),
            ),
            self.runner.worker_fallback_policy.as_deref(),
        );
        self.workspace_transport = self
            .runner
            .workspace_roots
            .as_ref()
            .map(|roots| roots.prepare(self.isolation.as_ref(), &self.owner, self.runner.deadline))
            .transpose()?;
        if let Some(transport) = &self.workspace_transport {
            transport.append_prompt(&mut args)?;
            self.lifecycle.workspace_roots = Some(transport.metadata.clone());
            crate::subagents::worker::bind_workspace_roots(&self.state, json!(transport.metadata))?;
        }
        self.prompt_snapshot = crate::subagents::task_file::snapshot_prompt(
            &mut args,
            &self.owner,
            self.runner.deadline,
        )?;
        if let Some(transport) = &self.workspace_transport {
            transport.append_args(&mut args)?;
        }
        if crate::subagents::worker::yield_results(&self.state) {
            args.push("--worker-yield".into());
        }
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

    async fn collect_turn(
        &mut self,
        message: &str,
        yield_keys: Option<(&str, &[u64])>,
        mut on_yield: impl FnMut(&str, u64, BatchAnswer) -> Result<()>,
    ) -> Result<(String, String, Option<Vec<u64>>)> {
        self.diagnostic.clear();
        let id = self.queue("prompt", Some(message))?;
        let mut reply = TurnReply::new(id.clone());
        if let Some((request_id, keys)) = yield_keys {
            if request_id != id {
                return Err(Error::tool("subagent", INVALID_RPC));
            }
            reply.yields = Some(YieldReplies {
                remaining: keys.to_vec(),
                calls: BTreeSet::new(),
            });
        }
        let mut output = String::new();
        loop {
            self.tick(|line, sent| {
                if let Some((key, answer)) = reply.ingest(line, sent, &mut output)? {
                    // Publish in frame order, before the same read batch can
                    // observe a later protocol failure or cancellation.
                    on_yield(&id, key, answer)?;
                    reply.accept_yield(key)?;
                }
                Ok(())
            })?;
            if reply.complete {
                break;
            }
            self.owner.time().sleep(POLL_INTERVAL).await;
        }
        // The physical group stays active through persistence/compaction even
        // when every logical item has already yielded. Cancellation preserves
        // accepted yields and settles only the remaining unfinished items.
        self.idle_barrier().await?;
        self.check()?;
        Ok((id, output, reply.yields.map(|yields| yields.remaining)))
    }

    fn completed_result(&self, assignment: String) -> SubagentResult {
        let mut result = SubagentResult::starting(
            &self.agent,
            task(&self.agent, assignment),
            None,
            &self.runner.child_binary,
            &self.runner.cwd,
            &[],
        );
        result.pid = self.lifecycle.pid;
        result.hub_id.clone_from(&self.lifecycle.hub_id);
        result.session_isolation = "persistent_rpc_worker";
        result.status = SubagentStatus::Completed;
        result.stderr.clone_from(&self.diagnostic);
        result
            .workspace_roots
            .clone_from(&self.lifecycle.workspace_roots);
        result
    }

    async fn turn(&mut self, pending: crate::subagents::worker::PendingBatch) -> Result<()> {
        // The validator is owned outside the mutable actor borrow so receipt
        // publication can run synchronously inside the pipe-draining callback.
        let contract = self.output_contract.take();
        let result = self.turn_with_contract(pending, contract.as_ref()).await;
        self.output_contract = contract;
        result
    }

    async fn turn_with_contract(
        &mut self,
        pending: crate::subagents::worker::PendingBatch,
        contract: Option<&OutputContract>,
    ) -> Result<()> {
        use crate::subagents::worker::output_contract::{
            batch_answers, batch_prompt, remaining_answers, yield_prompt,
        };

        let first = pending
            .turns
            .first()
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        if !pending.keyed && pending.turns.len() != 1 {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        if pending.yield_results && (!pending.keyed || self.isolation.is_some()) {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        let ids = pending.turns.iter().map(|turn| turn.id).collect::<Vec<_>>();
        // queue() consumes this exact next sequence synchronously before the
        // first await. Native turn keys and this request ID cannot be supplied
        // or changed by the child.
        let yield_request = pending
            .yield_results
            .then(|| {
                self.request_sequence
                    .checked_add(1)
                    .map(|sequence| format!("warm-{sequence}"))
                    .ok_or_else(|| Error::tool("subagent", INVALID_RPC))
            })
            .transpose()?;
        let message = if pending.keyed {
            let assignments = pending
                .turns
                .iter()
                .map(|turn| (turn.id, turn.task.as_str()))
                .collect::<Vec<_>>();
            if let Some(request_id) = &yield_request {
                yield_prompt(&assignments, contract, request_id)?
            } else {
                batch_prompt(&assignments, contract)?
            }
        } else {
            contract.map_or_else(
                || first.task.clone(),
                |contract| contract.prompt(&first.task),
            )
        };
        let templates = if pending.yield_results {
            pending
                .turns
                .iter()
                .map(|turn| {
                    let mut result = self.completed_result(turn.task.clone());
                    // The new prompt has not begun yet. Never copy a prior
                    // physical turn's diagnostics into an early result.
                    result.stderr.clear();
                    (turn.id, result)
                })
                .collect::<BTreeMap<_, _>>()
        } else {
            BTreeMap::new()
        };
        let state = self.state.clone();
        let workspace_lease = self
            .workspace_transport
            .as_ref()
            .map(|transport| transport.lease.clone());
        let (request_id, mut output, remaining) = self
            .collect_turn(
                &message,
                yield_request
                    .as_deref()
                    .map(|request_id| (request_id, ids.as_slice())),
                |request_id, key, answer| {
                    let mut result = templates
                        .get(&key)
                        .cloned()
                        .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
                    answer.apply(&mut result, contract);
                    let mut output = turn_output(result, key, Some((request_id, &ids)));
                    if let Some(details) = output.details.as_mut() {
                        details["workerBatch"]["yielded"] = json!(true);
                    }
                    if let Some(lease) = &workspace_lease {
                        lease.check()?;
                    }
                    if !crate::subagents::worker::complete_turn(
                        &state,
                        key,
                        Ok(output),
                        workspace_lease.as_ref(),
                    ) {
                        return Err(Error::tool("subagent", CANCELLED));
                    }
                    Ok(())
                },
            )
            .await?;
        let remaining = remaining.as_deref().unwrap_or(&ids);
        // Validate every envelope row and its key before accepting any item.
        // A malformed group fails all its members without retaining a raw
        // prefix. In yield mode earlier accepted rows are immutable; only the
        // final fallback's remaining keys belong to this envelope.
        let mut answers = pending.keyed.then(|| {
            let answers = if pending.yield_results {
                remaining_answers(&output, remaining)
            } else {
                batch_answers(&output, &ids)
            };
            answers
                .map(Vec::into_iter)
                .map_err(|error| error.to_string())
        });
        if pending.yield_results
            && let Some(Err(error)) = &answers
        {
            // Even an all-yielded batch still owes a valid terminal response
            // and idle barrier. A bad tail fails its carrier, not old results.
            return Err(Error::tool("subagent", error.clone()));
        }
        let isolation = self
            .isolation
            .as_ref()
            .map(crate::subagents::worker::isolation::outcome);
        self.check()?;
        for turn in pending.turns {
            if !remaining.contains(&turn.id) {
                continue;
            }
            let mut result = self.completed_result(turn.task);
            if let Some(answers) = &mut answers {
                match answers {
                    Ok(answers) => answers
                        .next()
                        .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?
                        .apply(&mut result, contract),
                    Err(error) => result.fail(error.clone()),
                }
            } else {
                result.output = std::mem::take(&mut output);
                if let Some(contract) = contract {
                    contract.validate(&mut result);
                }
            }
            if let Some(isolation) = &isolation {
                match isolation {
                    Ok(outcome) => result.iso = Some(outcome.clone()),
                    Err(error) => result.fail(format!(
                        "Failed to collect cumulative worktree diff; files are preserved: {error}"
                    )),
                }
            }
            let output = turn_output(
                result,
                turn.id,
                pending
                    .keyed
                    .then_some((request_id.as_str(), ids.as_slice())),
            );
            // Each original ID keeps its own privacy projection, dependency,
            // retention budget and cancellation-linearized publication fence.
            if let Some(lease) = &workspace_lease {
                lease.check()?;
            }
            if !crate::subagents::worker::complete_turn(
                &self.state,
                turn.id,
                Ok(output),
                workspace_lease.as_ref(),
            ) {
                return Err(Error::tool("subagent", CANCELLED));
            }
        }
        crate::subagents::worker::finish_batch(&self.state)
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
            if let Some(batch) = crate::subagents::worker::next_batch(&self.state) {
                self.turn(batch).await?;
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
fn turn_output(result: SubagentResult, key: u64, batch: Option<(&str, &[u64])>) -> ToolOutput {
    let mut output = result_output(&[result], "worker", false);
    if let Some(details) = output.details.as_mut() {
        details["sessionIsolation"] = json!("persistent_rpc_worker");
        details["turnId"] = json!(key);
        if let Some((request_id, ids)) = batch {
            details["workerBatch"] = json!({
                "schema":"pi.subagent.worker.batch.v1",
                "requestId":request_id, "key":key, "turnIds":ids,
                "patchScope":"cumulative_worker_batch",
            });
        }
    }
    output
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
        drop(self.prompt_snapshot.take());
        drop(self.workspace_transport.take());
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
struct YieldReplies {
    remaining: Vec<u64>,
    // Starts may be emitted for an entire tool batch before any end event.
    // Retire each entry on its end so invalid calls cannot grow retention.
    calls: BTreeSet<String>,
}

#[cfg(any(unix, windows))]
struct TurnReply {
    id: String,
    acknowledged: bool,
    started: bool,
    complete: bool,
    protocol: protocol::ChildProtocol,
    yields: Option<YieldReplies>,
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
            yields: None,
        }
    }

    fn ingest(
        &mut self,
        line: &str,
        sent: bool,
        output: &mut String,
    ) -> Result<Option<(u64, BatchAnswer)>> {
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
            return Ok(None);
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
        if kind == "agent_end"
            && self
                .yields
                .as_ref()
                .is_some_and(|yields| !yields.calls.is_empty())
        {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        let yielded = self.yield_event(line, &event)?;
        self.protocol
            .ingest(line, output)
            .map_err(|error| Error::tool("subagent", error))?;
        if kind == "agent_end" {
            self.protocol
                .finish()
                .map_err(|error| Error::tool("subagent", error))?;
            self.complete = true;
        }
        Ok(yielded)
    }

    fn yield_event(&mut self, line: &str, event: &Value) -> Result<Option<(u64, BatchAnswer)>> {
        use crate::subagents::worker::output_contract::{MAX_YIELD_EVENT_BYTES, yield_answer};

        let Some(yields) = &mut self.yields else {
            return Ok(None);
        };
        if event.get("toolName").and_then(Value::as_str) != Some("yield") {
            return Ok(None);
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(kind, "tool_execution_start" | "tool_execution_end") {
            return Ok(None);
        }
        if !self.started {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        let call_id = event
            .get("toolCallId")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty() && id.len() <= 256 && !id.contains('\0'))
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        if kind == "tool_execution_start" {
            if yields.calls.len() >= 32 || !yields.calls.insert(call_id.to_string()) {
                return Err(Error::tool("subagent", INVALID_RPC));
            }
            return Ok(None);
        }
        if !yields.calls.remove(call_id) {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        let failed = event
            .get("isError")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        if failed {
            return Ok(None);
        }
        // Parsing the complete bounded success frame again rejects duplicate
        // JSON fields, including inside data, before its typed claim is used.
        let event = crate::judgment::decode_bounded_json(line, MAX_YIELD_EVENT_BYTES)
            .map_err(|_| Error::tool("subagent", INVALID_RPC))?;
        if event
            .pointer("/result/isError")
            .is_some_and(|value| value != &Value::Bool(false))
        {
            return Err(Error::tool("subagent", INVALID_RPC));
        }
        let receipt = event
            .pointer("/result/details")
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        yield_answer(receipt, &self.id, call_id, &yields.remaining).map(Some)
    }

    fn accept_yield(&mut self, key: u64) -> Result<()> {
        let yields = self
            .yields
            .as_mut()
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        let index = yields
            .remaining
            .iter()
            .position(|candidate| *candidate == key)
            .ok_or_else(|| Error::tool("subagent", INVALID_RPC))?;
        yields.remaining.remove(index);
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

    const GROUPED_RPC_CHILD: &str = r#"
printf '%s\n' "$$" >> launches
count=0
while IFS= read -r line; do
    id=${line#*\"id\":\"}
    id=${id%%\"*}
    case "$line" in
        *'"type":"get_state"'*)
            if [ "$count" -eq 1 ] && [ ! -f release-idle ]; then
                printf ready > idle-blocked
                printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":true}}\n' "$id"
            else
                printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false,"contextTokens":42,"contextWindow":1000}}\n' "$id"
            fi
            ;;
        *'"type":"prompt"'*)
            count=$((count + 1))
            printf '%s\n' "$line" >> prompts
            printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
            printf '%s\n' '{"type":"agent_start"}'
            case "$count" in
                1) answer='{"items":[{"key":2,"data":{"value":"second"}},{"key":1,"data":{"value":"first"}}]}' ;;
                2) answer='{"items":[{"key":4,"data":{"value":"fourth"}},{"key":3,"error":"intentional-item-error"}]}' ;;
                3) answer='{"items":[{"key":5,"data":{"value":7}}]}' ;;
                *) answer='{"items":[{"key":6,"data":{"value":"sixth"}}]}' ;;
            esac
            escaped=$(printf '%s' "$answer" | sed 's/"/\\"/g')
            printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"%s"}]}]}\n' "$escaped"
            ;;
    esac
done
"#;

    const YIELD_RPC_CHILD: &str = r#"
printf '%s\n' "$$" >> launches
printf '%s\n' "$@" > arguments
count=0
while IFS= read -r line; do
    id=${line#*\"id\":\"}
    id=${id%%\"*}
    case "$line" in
        *'"type":"get_state"'*)
            if [ "$count" -gt 0 ] && [ -f bad-idle ]; then
                printf '%s\n' '{"type":"response","id":"foreign","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false}}'
            elif [ "$count" -eq 1 ] && [ ! -f release-idle ]; then
                printf ready > idle-blocked
                printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":true}}\n' "$id"
            else
                printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false,"contextTokens":42,"contextWindow":1000}}\n' "$id"
            fi
            ;;
        *'"type":"prompt"'*)
            count=$((count + 1))
            printf '%s\n' "$line" >> prompts
            printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
            printf '%s\n' '{"type":"agent_start"}'
            if [ "$count" -eq 1 ]; then
                value='{"value":"first"}'
                if [ -f schema-error ]; then value='{"value":7}'; fi
                if [ -f private-data ]; then value='{"value":"yield-privacy-secret"}'; fi
                printf '%s\n' '{"type":"tool_execution_start","toolName":"yield","toolCallId":"yield-1","args":{}}'
                printf '{"type":"tool_execution_end","toolName":"yield","toolCallId":"yield-1","isError":false,"result":{"content":[],"details":{"schema":"pi.subagent.worker.yield.v1","requestId":"%s","key":1,"data":%s,"toolCallId":"yield-1"}}}\n' "$id" "$value"
                if [ -f yield-all ]; then
                    printf '%s\n' '{"type":"tool_execution_start","toolName":"yield","toolCallId":"yield-2","args":{}}'
                    printf '{"type":"tool_execution_end","toolName":"yield","toolCallId":"yield-2","isError":false,"result":{"content":[],"details":{"schema":"pi.subagent.worker.yield.v1","requestId":"%s","key":2,"data":{"value":"second"},"toolCallId":"yield-2"}}}\n' "$id"
                fi
                printf ready > yielded
                while [ ! -f release-tail ]; do sleep 0.02; done
                if [ -f fail-after-yield ]; then
                    printf '%s\n' '{"type":"agent_end","error":"intentional late failure","messages":[]}'
                    continue
                fi
                if [ -f duplicate-tail ]; then
                    printf '%s\n' '{"type":"tool_execution_start","toolName":"yield","toolCallId":"yield-duplicate","args":{}}'
                    printf '{"type":"tool_execution_end","toolName":"yield","toolCallId":"yield-duplicate","isError":false,"result":{"content":[],"details":{"schema":"pi.subagent.worker.yield.v1","requestId":"%s","key":1,"data":{"value":"replacement-canary"},"toolCallId":"yield-duplicate"}}}\n' "$id"
                    continue
                fi
                if [ -f yield-all ]; then
                    answer='{"items":[]}'
                else
                    answer='{"items":[{"key":2,"data":{"value":"second"}}]}'
                fi
                if [ -f malformed-final ]; then answer='{"items":[{"key":1,"data":"replacement-canary"}]}'; fi
            else
                answer='{"items":[{"key":3,"data":{"value":"third"}}]}'
            fi
            escaped=$(printf '%s' "$answer" | sed 's/"/\\"/g')
            printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"%s"}]}]}\n' "$escaped"
            ;;
    esac
done
"#;

    fn pool_native_result(value: &Value) -> &Value {
        value
            .pointer("/item/result/details/results/0")
            .expect("original pool item must retain its own native result")
    }

    #[test]
    fn yielded_item_drains_before_terminal_answer_and_remaining_items_wait_for_idle() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(YIELD_RPC_CHILD);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = fixture
                .call(json!({
                    "operation":"pool_start", "agent":"worker", "concurrency":1,
                    "maxBatchSize":2, "yieldResults":true,
                    "tasks":["first-task", "second-task", "third-task"],
                    "outputSchema":{"type":"object", "required":["value"],
                        "additionalProperties":false, "properties":{"value":{"type":"string"}}}
                }))
                .await;
            let pool = field(&started, "poolId");
            let page = fixture
                .call(json!({"operation":"pool_drain", "poolId":pool,
                "timeoutMs":5000}))
                .await;
            assert_eq!(page["items"].as_array().unwrap().len(), 1);
            assert_eq!(page["items"][0]["id"], 1);
            assert_eq!(
                page["items"][0]["result"]["details"]["results"][0]["data"],
                json!({"value":"first"})
            );
            assert_eq!(
                page["items"][0]["result"]["details"]["workerBatch"]["yielded"],
                true
            );
            assert_eq!(page["done"], false);
            assert!(!fixture.root.path().join("release-tail").exists());
            let busy = fixture
                .call(json!({"operation":"pool_status", "poolId":pool}))
                .await;
            assert_eq!(busy["completedItems"], 1);
            assert_eq!(busy["runningItems"], 1);
            assert_eq!(busy["queuedItems"], 1);
            fixture
                .call(json!({"operation":"pool_finish", "poolId":pool}))
                .await;
            std::fs::write(fixture.root.path().join("release-tail"), "ready").unwrap();
            fixture.await_file("idle-blocked").await;
            let waiting = fixture
                .call(json!({"operation":"pool_result", "poolId":pool,
                "itemId":2}))
                .await;
            assert_eq!(waiting["item"]["status"], "running");
            assert_eq!(
                std::fs::read_to_string(fixture.root.path().join("prompts"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
            std::fs::write(fixture.root.path().join("release-idle"), "ready").unwrap();
            let settled = fixture
                .call(json!({"operation":"pool_wait", "poolId":pool,
                "timeoutMs":5000}))
                .await;
            assert_eq!(settled["status"], "completed");
            assert_eq!(settled["completedItems"], 3);
            for (id, expected) in [(1, "first"), (2, "second"), (3, "third")] {
                let item = fixture
                    .call(json!({"operation":"pool_result", "poolId":pool,
                    "itemId":id}))
                    .await;
                assert_eq!(pool_native_result(&item)["data"], json!({"value":expected}));
                assert_eq!(pool_native_result(&item)["schemaValid"], true);
                assert_eq!(
                    pool_native_result(&item)["task"],
                    format!("{expected}-task")
                );
            }
            let prompts = std::fs::read_to_string(fixture.root.path().join("prompts")).unwrap();
            let prompts: Vec<Value> = prompts
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(prompts.len(), 2);
            assert!(
                prompts[0]["message"]
                    .as_str()
                    .unwrap()
                    .contains("native yield tool")
            );
            let arguments = std::fs::read_to_string(fixture.root.path().join("arguments")).unwrap();
            assert!(
                arguments
                    .lines()
                    .any(|argument| argument == "--worker-yield")
            );
        });
    }

    #[test]
    fn all_yielded_items_do_not_close_a_sealed_worker_before_terminal_and_idle() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(YIELD_RPC_CHILD);
        std::fs::write(fixture.root.path().join("yield-all"), "ready").unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = fixture
                .call(json!({"operation":"pool_start", "agent":"worker",
                "concurrency":1, "maxBatchSize":2, "yieldResults":true,
                "tasks":["first-task", "second-task"]}))
                .await;
            let pool = field(&started, "poolId");
            let second = fixture
                .call(json!({"operation":"pool_wait", "poolId":pool,
                "itemId":2, "timeoutMs":5000}))
                .await;
            assert_eq!(second["item"]["status"], "completed");
            let pid = std::fs::read_to_string(fixture.root.path().join("launches"))
                .unwrap()
                .trim()
                .parse::<i32>()
                .ok()
                .and_then(rustix::process::Pid::from_raw)
                .unwrap();
            let sealed = fixture
                .call(json!({"operation":"pool_finish", "poolId":pool}))
                .await;
            assert_eq!(sealed["status"], "draining");
            let still_running = fixture
                .call(json!({"operation":"pool_wait", "poolId":pool,
                "timeoutMs":40}))
                .await;
            assert_eq!(still_running["status"], "draining");
            assert_eq!(still_running["completedItems"], 2);
            assert!(rustix::process::test_kill_process(pid).is_ok());
            std::fs::write(fixture.root.path().join("release-tail"), "ready").unwrap();
            fixture.await_file("idle-blocked").await;
            let compacting = fixture
                .call(json!({"operation":"pool_wait", "poolId":pool,
                "timeoutMs":40}))
                .await;
            assert_eq!(compacting["status"], "draining");
            assert!(rustix::process::test_kill_process(pid).is_ok());
            std::fs::write(fixture.root.path().join("release-idle"), "ready").unwrap();
            let completed = fixture
                .call(json!({"operation":"pool_wait", "poolId":pool,
                "timeoutMs":5000}))
                .await;
            assert_eq!(completed["status"], "completed");
            assert_eq!(completed["completedItems"], 2);
            assert!(rustix::process::test_kill_process(pid).is_err());
        });
    }

    #[test]
    fn late_failure_duplicate_receipt_and_cancellation_preserve_accepted_yields() {
        let _guard = crate::jobs::process_test_guard();
        for failure in [
            "cancel",
            "fail-after-yield",
            "duplicate-tail",
            "malformed-final",
        ] {
            let fixture = Fixture::new(YIELD_RPC_CHILD);
            std::fs::write(fixture.root.path().join(failure), "ready").unwrap();
            std::fs::write(fixture.root.path().join("release-idle"), "ready").unwrap();
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let started = fixture
                    .call(json!({"operation":"pool_start", "agent":"worker",
                    "concurrency":1, "maxBatchSize":2, "yieldResults":true,
                    "tasks":["first-task", "second-task", "third-task"]}))
                    .await;
                let pool = field(&started, "poolId");
                let first = fixture
                    .call(json!({"operation":"pool_wait", "poolId":pool,
                    "itemId":1, "timeoutMs":5000}))
                    .await;
                assert_eq!(first["item"]["status"], "completed");
                let original = first["item"]["result"].clone();
                let settled = if failure == "cancel" {
                    fixture
                        .call(json!({"operation":"pool_close", "poolId":pool}))
                        .await
                } else {
                    std::fs::write(fixture.root.path().join("release-tail"), "ready").unwrap();
                    fixture
                        .call(json!({"operation":"pool_wait", "poolId":pool,
                        "timeoutMs":5000}))
                        .await
                };
                assert_eq!(
                    settled["status"],
                    if failure == "cancel" {
                        "cancelled"
                    } else {
                        "failed"
                    }
                );
                assert_eq!(settled["completedItems"], 1);
                assert_eq!(settled["failedItems"], 2);
                let retained = fixture
                    .call(json!({"operation":"pool_result", "poolId":pool,
                    "itemId":1}))
                    .await;
                assert_eq!(retained["item"]["result"], original);
                for id in [2, 3] {
                    let item = fixture
                        .call(json!({"operation":"pool_result", "poolId":pool,
                        "itemId":id}))
                        .await;
                    assert!(matches!(
                        item["item"]["status"].as_str(),
                        Some("failed" | "cancelled")
                    ));
                    assert!(!item.to_string().contains("replacement-canary"));
                }
                assert_eq!(
                    std::fs::read_to_string(fixture.root.path().join("prompts"))
                        .unwrap()
                        .lines()
                        .count(),
                    1
                );
            });
        }
    }

    #[test]
    fn all_yielded_successes_survive_a_failed_carrier_tail() {
        let _guard = crate::jobs::process_test_guard();
        for failure in ["bad-idle", "malformed-final"] {
            let fixture = Fixture::new(YIELD_RPC_CHILD);
            for marker in ["yield-all", "release-idle", failure] {
                std::fs::write(fixture.root.path().join(marker), "ready").unwrap();
            }
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let started = fixture
                    .call(json!({"operation":"pool_start", "agent":"worker",
                    "concurrency":1, "maxBatchSize":2, "yieldResults":true,
                    "tasks":["first-task", "second-task"]}))
                    .await;
                let pool = field(&started, "poolId");
                let second = fixture
                    .call(json!({"operation":"pool_wait", "poolId":pool,
                    "itemId":2, "timeoutMs":5000}))
                    .await;
                assert_eq!(second["item"]["status"], "completed");
                fixture
                    .call(json!({"operation":"pool_finish", "poolId":pool}))
                    .await;
                std::fs::write(fixture.root.path().join("release-tail"), "ready").unwrap();
                let settled = fixture
                    .call(json!({"operation":"pool_wait", "poolId":pool,
                    "timeoutMs":5000}))
                    .await;
                assert_eq!(settled["status"], "failed");
                assert_eq!(settled["completedItems"], 2);
                assert_eq!(settled["failedItems"], 0);
                for id in [1, 2] {
                    let item = fixture
                        .call(json!({"operation":"pool_result", "poolId":pool,
                        "itemId":id}))
                        .await;
                    assert_eq!(item["item"]["status"], "completed");
                    assert!(!item.to_string().contains("replacement-canary"));
                }
            });
        }
    }

    #[test]
    fn yielded_schema_and_privacy_failures_settle_only_their_original_item() {
        let _guard = crate::jobs::process_test_guard();
        for failure in ["schema-error", "private-data"] {
            let fixture = Fixture::new(YIELD_RPC_CHILD);
            std::fs::write(fixture.root.path().join(failure), "ready").unwrap();
            std::fs::write(fixture.root.path().join("release-idle"), "ready").unwrap();
            let mut vault = crate::secrets::SecretVault::default();
            if failure == "private-data" {
                let secret = "yield-privacy-secret";
                crate::secrets::obfuscate(
                    secret,
                    &mut vault,
                    &[regex::Regex::new(secret).unwrap()],
                );
            }
            let privacy = crate::text_completion::AuxiliaryPrivacySnapshot::with_vault(
                crate::text_completion::AuxiliaryPrivacy::from_settings(None),
                &vault,
            );
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let started = crate::subagents::worker::execute(
                    &fixture.tool,
                    json!({
                        "operation":"pool_start", "agent":"worker", "concurrency":1,
                        "maxBatchSize":2, "yieldResults":true,
                        "tasks":["first-task", "second-task"],
                        "outputSchema":{"type":"object", "required":["value"],
                            "additionalProperties":false, "properties":{"value":{"type":"string"}}}
                    }),
                    privacy,
                )
                .await
                .unwrap()
                .details
                .unwrap();
                let pool = field(&started, "poolId");
                let failed = fixture
                    .call(json!({"operation":"pool_wait", "poolId":pool,
                    "itemId":1, "timeoutMs":5000}))
                    .await;
                assert_eq!(failed["item"]["status"], "failed");
                assert!(!failed.to_string().contains("yield-privacy-secret"));
                if failure == "schema-error" {
                    assert_eq!(pool_native_result(&failed)["schemaValid"], false);
                    assert_eq!(pool_native_result(&failed)["schemaRetries"], 0);
                } else {
                    assert!(
                        failed
                            .to_string()
                            .contains("PI_SUBAGENT_WORKER_RESULT_WITHHELD")
                    );
                }
                fixture
                    .call(json!({"operation":"pool_finish", "poolId":pool}))
                    .await;
                std::fs::write(fixture.root.path().join("release-tail"), "ready").unwrap();
                let settled = fixture
                    .call(json!({"operation":"pool_wait", "poolId":pool,
                    "timeoutMs":5000}))
                    .await;
                assert_eq!(settled["status"], "failed");
                assert_eq!(settled["completedItems"], 1);
                assert_eq!(settled["failedItems"], 1);
                let second = fixture
                    .call(json!({"operation":"pool_result", "poolId":pool,
                    "itemId":2}))
                    .await;
                assert_eq!(second["item"]["status"], "completed");
                assert_eq!(
                    pool_native_result(&second)["data"],
                    json!({"value":"second"})
                );
            });
        }
    }

    #[test]
    fn grouped_pool_turns_preserve_item_results_errors_and_the_idle_barrier() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(GROUPED_RPC_CHILD);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = fixture.call(json!({
                "operation":"pool_start", "agent":"worker", "concurrency":1,
                "maxBatchSize":2, "tasks":["first-task", "second-task", "third-task", "fourth-task", "fifth-task"],
                "outputSchema":{"type":"object", "required":["value"],
                    "additionalProperties":false, "properties":{"value":{"type":"string"}}}
            })).await;
            let pool = field(&started, "poolId");
            fixture.await_file("idle-blocked").await;
            let blocked = fixture.call(json!({"operation":"pool_status", "poolId":pool})).await;
            assert_eq!(blocked["completedItems"], 0);
            assert_eq!(blocked["runningItems"], 2);
            assert_eq!(blocked["queuedItems"], 3);
            assert_eq!(blocked["maxBatchSize"], 2);
            std::fs::write(fixture.root.path().join("release-idle"), "ready").unwrap();
            let fifth = fixture.call(json!({"operation":"pool_wait", "poolId":pool,
                "itemId":5, "timeoutMs":5000})).await;
            assert_eq!(fifth["item"]["status"], "failed");
            assert_eq!(pool_native_result(&fifth)["schemaValid"], false);
            assert_eq!(pool_native_result(&fifth)["schemaRetries"], 0);
            assert_eq!(pool_native_result(&fifth)["task"], "fifth-task");

            let appended = fixture.call(json!({"operation":"pool_push", "poolId":pool,
                "tasks":["sixth-task"]})).await;
            assert_eq!(appended["admittedItemIds"], json!([6]));
            fixture.call(json!({"operation":"pool_finish", "poolId":pool})).await;
            let settled = fixture.call(json!({"operation":"pool_wait", "poolId":pool,
                "timeoutMs":5000})).await;
            assert_eq!(settled["status"], "failed");
            assert_eq!(settled["completedItems"], 4);
            assert_eq!(settled["failedItems"], 2);
            for (id, expected) in [(1, "first"), (2, "second"), (4, "fourth"), (6, "sixth")] {
                let item = fixture.call(json!({"operation":"pool_result", "poolId":pool,
                    "itemId":id})).await;
                let result = pool_native_result(&item);
                assert_eq!(result["data"], json!({"value":expected}));
                assert_eq!(result["schemaValid"], true);
                assert_eq!(result["task"], format!("{expected}-task"));
                assert_eq!(item["item"]["result"]["details"]["workerBatch"]["key"], id);
                assert!(!result["output"].as_str().unwrap().contains("\"items\""));
            }
            let failed = fixture.call(json!({"operation":"pool_result", "poolId":pool,
                "itemId":3})).await;
            assert_eq!(failed["item"]["status"], "failed");
            assert_eq!(pool_native_result(&failed)["error"], "intentional-item-error");
            assert!(pool_native_result(&failed).get("data").is_none());

            let page = fixture.call(json!({"operation":"pool_drain", "poolId":pool,
                "timeoutMs":0})).await;
            assert_eq!(page["items"].as_array().unwrap().iter()
                .map(|item| item["id"].as_u64().unwrap()).collect::<Vec<_>>(), vec![1,2,3,4]);
            let replay = fixture.call(json!({"operation":"pool_drain", "poolId":pool,
                "timeoutMs":0})).await;
            assert_eq!(page["items"], replay["items"]);
            let tail = fixture.call(json!({"operation":"pool_drain", "poolId":pool,
                "cursor":page["nextCursor"], "timeoutMs":0})).await;
            assert_eq!(tail["items"].as_array().unwrap().iter()
                .map(|item| item["id"].as_u64().unwrap()).collect::<Vec<_>>(), vec![5,6]);

            let prompts = std::fs::read_to_string(fixture.root.path().join("prompts")).unwrap();
            let prompts: Vec<Value> = prompts.lines()
                .map(|line| serde_json::from_str(line).unwrap()).collect();
            assert_eq!(prompts.len(), 4, "six logical items must use four physical prompts");
            let first = prompts[0]["message"].as_str().unwrap();
            assert!(first.contains("\"key\":1") && first.contains("\"key\":2"));
            assert!(first.contains("first-task") && first.contains("second-task"));
            assert!(!first.contains("third-task"));
            assert!(!first.contains("Output contract for this turn:"));
            assert!(prompts[2]["message"].as_str().unwrap().contains("\"key\":5"));
            assert!(prompts[3]["message"].as_str().unwrap().contains("\"key\":6"));
            assert_eq!(std::fs::read_to_string(fixture.root.path().join("launches"))
                .unwrap().lines().count(), 1);
        });
    }

    #[test]
    fn malformed_grouped_reply_fails_every_member_without_replaying_the_turn() {
        let _guard = crate::jobs::process_test_guard();
        for answer in [
            json!({"items":[{"key":1,"data":"unaccepted-canary"}]}),
            json!({"items":[{"key":1,"data":"unaccepted-canary"},{"key":1,"data":false}]}),
            json!({"items":[{"key":1,"data":"unaccepted-canary"},{"key":3,"data":false}]}),
        ] {
            let frame = json!({"type":"agent_end", "messages":[{
                "role":"assistant", "stopReason":"stop", "content":[{
                    "type":"text", "text":serde_json::to_string(&answer).unwrap()
                }]
            }]})
            .to_string();
            let script = format!(
                r#"
while IFS= read -r line; do
    id=${{line#*\"id\":\"}}
    id=${{id%%\"*}}
    case "$line" in
        *'"type":"get_state"'*)
            printf '{{"type":"response","id":"%s","command":"get_state","success":true,"data":{{"isStreaming":false,"isCompacting":false}}}}\n' "$id"
            ;;
        *'"type":"prompt"'*)
            printf '%s\n' "$line" >> prompts
            printf '{{"type":"response","id":"%s","command":"prompt","success":true}}\n' "$id"
            printf '%s\n' '{{"type":"agent_start"}}'
            printf '%s\n' '{frame}'
            ;;
    esac
done
"#
            );
            let fixture = Fixture::new(&script);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let started = fixture
                    .call(json!({"operation":"pool_start", "agent":"worker",
                    "concurrency":1, "maxBatchSize":2, "tasks":["left-task", "right-task"]}))
                    .await;
                let pool = field(&started, "poolId");
                fixture
                    .call(json!({"operation":"pool_finish", "poolId":pool}))
                    .await;
                let settled = fixture
                    .call(json!({"operation":"pool_wait", "poolId":pool,
                    "timeoutMs":5000}))
                    .await;
                assert_eq!(settled["completedItems"], 0);
                assert_eq!(settled["failedItems"], 2);
                for id in [1, 2] {
                    let item = fixture
                        .call(json!({"operation":"pool_result", "poolId":pool,
                        "itemId":id}))
                        .await;
                    let result = pool_native_result(&item);
                    assert!(
                        result["error"]
                            .as_str()
                            .unwrap()
                            .contains("PI_SUBAGENT_WORKER_BATCH_OUTPUT")
                    );
                    assert!(result.get("data").is_none());
                    assert!(!item.to_string().contains("unaccepted-canary"));
                }
                assert_eq!(
                    std::fs::read_to_string(fixture.root.path().join("prompts"))
                        .unwrap()
                        .lines()
                        .count(),
                    1
                );
            });
        }
    }

    #[test]
    fn cancelling_a_group_at_the_idle_barrier_retains_no_terminal_prefix() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(GROUPED_RPC_CHILD);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = fixture.call(json!({"operation":"pool_start", "agent":"worker",
                "concurrency":1, "maxBatchSize":2, "tasks":["first-task", "second-task", "third-task"]})).await;
            let pool = field(&started, "poolId");
            fixture.await_file("idle-blocked").await;
            let pid = std::fs::read_to_string(fixture.root.path().join("launches"))
                .unwrap()
                .trim()
                .parse::<i32>()
                .ok()
                .and_then(rustix::process::Pid::from_raw)
                .unwrap();
            assert!(rustix::process::test_kill_process(pid).is_ok());
            let closed = fixture.call(json!({"operation":"pool_close", "poolId":pool})).await;
            assert_eq!(closed["status"], "cancelled");
            assert_eq!(closed["completedItems"], 0);
            assert_eq!(closed["failedItems"], 3);
            for id in [1,2,3] {
                let item = fixture.call(json!({"operation":"pool_result", "poolId":pool,
                    "itemId":id})).await;
                assert_eq!(item["item"]["status"], "cancelled");
                assert!(item["item"]["result"]["isError"].as_bool().unwrap());
                assert!(!item.to_string().contains("\"value\":\"first\""));
            }
            assert_eq!(std::fs::read_to_string(fixture.root.path().join("prompts"))
                .unwrap().lines().count(), 1);
            assert!(rustix::process::test_kill_process(pid).is_err());
        });
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn retained_workers_and_pools_keep_root_grants_until_revocation_reaps_them() {
        let _guard = crate::jobs::process_test_guard();
        let script = format!(
            r#"set -eu
while [ "$#" -gt 0 ]; do
    case "$1" in
        --workspace-roots-file)
            shift
            test -f "$1"
            printf '%s' "$1" > root-manifest-path
            cat "$1" > root-manifest
            ;;
        --append-system-prompt)
            shift
            cat "$1" > root-system-prompt
            ;;
    esac
    shift
done
{RPC_CHILD}
"#,
        );
        for pooled in [false, true] {
            let mut fixture = Fixture::new(&script);
            let additional = tempfile::tempdir().expect("additional repository");
            let later = tempfile::tempdir().expect("later repository");
            let mut workspace = crate::workspace::WorkspaceHandle::single(fixture.root.path());
            workspace.add_root(additional.path());
            fixture.tool.workspace = workspace.clone();
            std::fs::write(fixture.root.path().join("release"), "ready").unwrap();
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let started = if pooled {
                    fixture
                        .call(json!({"operation":"pool_start","agent":"worker",
                        "concurrency":1,"tasks":["remember silver"]}))
                        .await
                } else {
                    fixture
                        .call(json!({"operation":"worker_start","agent":"worker",
                        "task":"remember silver"}))
                        .await
                };
                let id = field(&started, if pooled { "poolId" } else { "workerId" });
                let result_path = if pooled {
                    "/item/result/details/results/0"
                } else {
                    "/turn/result/details/results/0"
                };
                let first = if pooled {
                    fixture
                        .call(json!({"operation":"pool_wait","poolId":id,
                        "itemId":1,"timeoutMs":5000}))
                        .await
                } else {
                    fixture.wait(&id, 1).await
                };
                let record = first.pointer(result_path).expect("native completed child");
                assert_eq!(record["output"], "silver");
                let roots = json!([crate::extensions::safe_canonicalize(additional.path())]);
                assert_eq!(record["workspaceRoots"]["inherited"], roots);
                let pid = i32::try_from(record["pid"].as_u64().unwrap()).unwrap();
                let pid = rustix::process::Pid::from_raw(pid).unwrap();
                let manifest_path = std::path::PathBuf::from(
                    std::fs::read_to_string(fixture.root.path().join("root-manifest-path"))
                        .unwrap(),
                );
                assert!(manifest_path.is_file());
                let manifest: Value = serde_json::from_slice(
                    &std::fs::read(fixture.root.path().join("root-manifest")).unwrap(),
                )
                .unwrap();
                assert_eq!(manifest["roots"], roots);
                workspace.add_root(later.path());
                let second = if pooled {
                    fixture
                        .call(json!({"operation":"pool_push","poolId":id,
                        "tasks":["recall the remembered word"]}))
                        .await;
                    fixture
                        .call(json!({"operation":"pool_wait","poolId":id,
                        "itemId":2,"timeoutMs":5000}))
                        .await
                } else {
                    fixture
                        .call(json!({"operation":"worker_send","workerId":id,
                        "task":"recall the remembered word"}))
                        .await;
                    fixture.wait(&id, 2).await
                };
                let second = second.pointer(result_path).unwrap();
                assert_eq!(second["output"], "silver");
                assert_eq!(second["workspaceRoots"]["inherited"], roots);
                assert!(
                    manifest_path.is_file(),
                    "live process still owns its manifest"
                );
                assert!(workspace.remove_root(additional.path()));
                workspace.add_root(additional.path());
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let status = if pooled {
                        fixture
                            .call(json!({"operation":"pool_status","poolId":id}))
                            .await
                    } else {
                        fixture
                            .call(json!({"operation":"worker_status","workerId":id}))
                            .await
                    };
                    let retired = if pooled {
                        status["workerWorkspaces"][0]["closed"] == true
                    } else {
                        status["status"] == "failed"
                    };
                    if retired {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "idle child retained revoked access"
                    );
                    AgentCx::for_request().time().sleep(POLL_INTERVAL).await;
                }
                assert!(rustix::process::test_kill_process(pid).is_err());
                assert!(
                    !manifest_path.exists(),
                    "retirement releases private grants file"
                );
                assert_eq!(
                    std::fs::read_to_string(fixture.root.path().join("launches"))
                        .unwrap()
                        .lines()
                        .count(),
                    1,
                    "revocation must never relaunch a worker under fresh grants"
                );
            });
        }
    }

    #[test]
    fn large_definition_snapshots_survive_worker_and_pool_reuse_then_retire() {
        let _guard = crate::jobs::process_test_guard();
        let script = format!(
            r#"set -eu
printf '%s\n' "$@" > arguments
while [ "$#" -gt 0 ]; do
    case "$1" in
        --append-system-prompt)
            shift
            test -f "$1"
            printf '%s' "$1" > prompt-path
            cat "$1" > system-instructions-used
            ;;
    esac
    shift
done
{RPC_CHILD}
"#,
        );
        for pooled in [false, true] {
            let fixture = Fixture::new(&script);
            let body = format!(
                "Frozen worker definition.\n{}",
                "résumé 雪\n".repeat(24 * 1024)
            );
            std::fs::write(
                fixture.root.path().join("global/agents/worker.md"),
                format!("---\nname: worker\ndescription: large retained instructions\ntools: []\n---\n{body}"),
            )
            .unwrap();
            std::fs::write(fixture.root.path().join("release"), "ready").unwrap();
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let started = if pooled {
                    fixture.call(json!({"operation":"pool_start", "agent":"worker",
                        "concurrency":1, "tasks":["remember silver", "recall the remembered word"]})).await
                } else {
                    fixture.call(json!({"operation":"worker_start", "agent":"worker",
                        "task":"remember silver"})).await
                };
                let id = field(&started, if pooled { "poolId" } else { "workerId" });
                let first = if pooled {
                    fixture.call(json!({"operation":"pool_wait", "poolId":id,
                        "itemId":1, "timeoutMs":5000})).await
                } else {
                    fixture.wait(&id, 1).await
                };
                let result_path = if pooled {
                    "/item/result/details/results/0"
                } else {
                    "/turn/result/details/results/0"
                };
                assert_eq!(first.pointer(result_path).unwrap()["output"], "silver");
                let path = std::path::PathBuf::from(
                    std::fs::read_to_string(fixture.root.path().join("prompt-path")).unwrap(),
                );
                assert_eq!(std::fs::read_to_string(&path).unwrap(), body.trim());
                assert_eq!(
                    std::fs::read_to_string(fixture.root.path().join("system-instructions-used"))
                        .unwrap(),
                    body.trim()
                );
                let arguments =
                    std::fs::read_to_string(fixture.root.path().join("arguments")).unwrap();
                assert!(!arguments.contains("Frozen worker definition."));
                assert!(arguments.len() < 8192);
                std::fs::write(
                    fixture.root.path().join("global/agents/worker.md"),
                    "changed after the worker started",
                )
                .unwrap();
                let second = if pooled {
                    fixture.call(json!({"operation":"pool_wait", "poolId":id,
                        "itemId":2, "timeoutMs":5000})).await
                } else {
                    fixture.call(json!({"operation":"worker_send", "workerId":id,
                        "task":"recall the remembered word"})).await;
                    fixture.wait(&id, 2).await
                };
                let first = first.pointer(result_path).unwrap();
                let second = second.pointer(result_path).unwrap();
                assert_eq!(first["pid"], second["pid"]);
                assert_eq!(second["output"], "silver");
                assert_eq!(std::fs::read_to_string(&path).unwrap(), body.trim());
                if pooled {
                    fixture.call(json!({"operation":"pool_finish", "poolId":id})).await;
                    let settled = fixture.call(json!({"operation":"pool_wait", "poolId":id,
                        "timeoutMs":5000})).await;
                    assert_eq!(settled["status"], "completed");
                } else {
                    let closed = fixture.call(json!({"operation":"worker_close", "workerId":id})).await;
                    assert_eq!(closed["status"], "closed");
                }
                assert!(
                    !path.exists(),
                    "retirement must release the private prompt snapshot"
                );
            });
        }
    }

    fn isolation_git(root: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=worker fixture",
                "-c",
                "user.email=worker@example.test",
            ])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("real Git fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("Git fixture output")
    }

    fn prepare_isolated_fixture(fixture: &Fixture) {
        let root = fixture.root.path();
        isolation_git(root, &["init", "-q"]);
        std::fs::write(
            root.join(".gitignore"),
            "/global/\n/child.sh\n/launches\n/first-running\n/release\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/tracked.txt"), "committed source\n").unwrap();
        isolation_git(root, &["add", ".gitignore", "nested/tracked.txt"]);
        isolation_git(root, &["commit", "-qm", "baseline"]);
        std::fs::write(root.join("nested/tracked.txt"), "staged source\n").unwrap();
        isolation_git(root, &["add", "nested/tracked.txt"]);
        std::fs::write(root.join("nested/tracked.txt"), "dirty source\n").unwrap();
        std::fs::write(root.join("nested/untracked.txt"), "source note\n").unwrap();
    }

    const ISOLATED_RPC_CHILD: &str = r#"
set -eu
control_root=$(dirname "$0")
printf '%s\n' "$$" >> "$control_root/launches"
while IFS= read -r line; do
    id=$(printf '%s\n' "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    case "$line" in
        *'"type":"get_state"'*)
            printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false}}\n' "$id"
            ;;
        *'"type":"prompt"'*)
            printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
            printf '%s\n' '{"type":"agent_start"}'
            test "$(cat tracked.txt)" = 'dirty source'
            test "$(cat untracked.txt)" = 'source note'
            if [ -f worker.txt ]; then
                test "$(cat worker.txt)" = 'first'
                printf 'second\n' >> worker.txt
                answer=second
            else
                printf 'first\n' > worker.txt
                answer=first
            fi
            case "$line" in
                *'Task: hold'*)
                    printf ready > "$control_root/first-running"
                    while [ ! -f "$control_root/release" ]; do sleep 0.02; done
                    ;;
            esac
            printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"%s"}]}]}\n' "$answer"
            ;;
    esac
done
"#;

    fn retained_handle(fixture: &Fixture, descriptor: &Value) -> crate::worktree_iso::IsoHandle {
        crate::worktree_iso::IsoHandle {
            id: field(descriptor, "branch"),
            branch: field(descriptor, "branch"),
            path: std::path::PathBuf::from(field(descriptor, "worktreePath")),
            repo_root: fixture.root.path().to_path_buf(),
            baseline: field(descriptor, "baseline"),
        }
    }

    #[test]
    fn isolated_worker_reuses_dirty_launch_state_and_retains_cumulative_edits() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(ISOLATED_RPC_CHILD);
        prepare_isolated_fixture(&fixture);
        let before_head = isolation_git(fixture.root.path(), &["rev-parse", "HEAD"]);
        let before_index = std::fs::read(fixture.root.path().join(".git/index")).unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = fixture
                .call(json!({
                    "operation":"worker_start", "agent":"worker", "task":"hold",
                    "cwd":"nested", "isolation":"worktree", "isoApply":"keep"
                }))
                .await;
            let worker = field(&started, "workerId");
            fixture.await_file("first-running").await;
            let live = fixture
                .call(json!({"operation":"worker_status", "workerId":worker}))
                .await;
            let descriptor = live["workspace"].clone();
            let handle = retained_handle(&fixture, &descriptor);
            assert_eq!(descriptor["schema"], "pi.subagent.worker.workspace.v1");
            assert_eq!(descriptor["applyMode"], "keep");
            assert_eq!(descriptor["applied"], false);
            assert_eq!(
                descriptor["cwd"].as_str(),
                handle.path.join("nested").to_str()
            );
            std::fs::write(
                fixture.root.path().join("nested/tracked.txt"),
                "later parent\n",
            )
            .unwrap();
            fixture
                .call(json!({"operation":"worker_send", "workerId":worker, "task":"continue"}))
                .await;
            std::fs::write(fixture.root.path().join("release"), "continue").unwrap();
            let first = fixture.wait(&worker, 1).await;
            let second = fixture.wait(&worker, 2).await;
            assert_eq!(native_result(&first)["pid"], native_result(&second)["pid"]);
            assert_eq!(native_result(&first)["output"], "first");
            assert_eq!(native_result(&second)["output"], "second");
            let first_patch = native_result(&first)["iso"]["patch"].as_str().unwrap();
            let second_patch = native_result(&second)["iso"]["patch"].as_str().unwrap();
            assert!(first_patch.contains("+first\n"));
            assert!(!first_patch.contains("+second\n"));
            assert!(second_patch.contains("+first\n+second\n"));
            assert!(!second_patch.contains("tracked.txt"));
            assert!(!second_patch.contains("untracked.txt"));
            let pid = native_result(&second)["pid"]
                .as_i64()
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(rustix::process::Pid::from_raw)
                .unwrap();
            let closed = fixture
                .call(json!({"operation":"worker_close", "workerId":worker}))
                .await;
            assert_eq!(closed["workspace"], descriptor);
            assert!(rustix::process::test_kill_process(pid).is_err());
            assert_eq!(
                std::fs::read_to_string(handle.path.join("nested/worker.txt")).unwrap(),
                "first\nsecond\n"
            );
            assert!(!fixture.root.path().join("nested/worker.txt").exists());
            assert_eq!(
                std::fs::read_to_string(fixture.root.path().join("nested/tracked.txt")).unwrap(),
                "later parent\n"
            );
            assert_eq!(
                isolation_git(fixture.root.path(), &["rev-parse", "HEAD"]),
                before_head
            );
            assert_eq!(
                std::fs::read(fixture.root.path().join(".git/index")).unwrap(),
                before_index
            );
            crate::worktree_iso::drop_worktree(&handle).expect("retire asserted test fixture");
        });
    }

    #[test]
    fn cancelled_isolated_worker_keeps_partial_edits_and_recovery_identity() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(ISOLATED_RPC_CHILD);
        prepare_isolated_fixture(&fixture);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = fixture
                .call(json!({
                    "operation":"worker_start", "agent":"worker", "task":"hold",
                    "cwd":"nested", "isolation":"worktree"
                }))
                .await;
            let worker = field(&started, "workerId");
            fixture.await_file("first-running").await;
            let live = fixture
                .call(json!({"operation":"worker_status", "workerId":worker}))
                .await;
            let descriptor = live["workspace"].clone();
            let handle = retained_handle(&fixture, &descriptor);
            let closed = fixture
                .call(json!({"operation":"worker_close", "workerId":worker}))
                .await;
            assert_eq!(closed["workspace"], descriptor);
            assert_eq!(closed["turn"]["status"], "cancelled");
            assert_eq!(
                std::fs::read_to_string(handle.path.join("nested/worker.txt")).unwrap(),
                "first\n"
            );
            assert!(!fixture.root.path().join("nested/worker.txt").exists());
            crate::worktree_iso::drop_worktree(&handle).expect("retire asserted test fixture");
        });
    }

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

    fn yielding_reply() -> TurnReply {
        let mut reply = TurnReply::new("warm-2".to_string());
        reply.yields = Some(YieldReplies {
            remaining: vec![1, 2],
            calls: BTreeSet::new(),
        });
        reply
            .ingest(&acknowledged("warm-2"), true, &mut String::new())
            .unwrap();
        reply
            .ingest(r#"{"type":"agent_start"}"#, true, &mut String::new())
            .unwrap();
        reply
    }

    fn yield_started(call: &str) -> String {
        json!({"type":"tool_execution_start", "toolName":"yield",
            "toolCallId":call, "args":{}})
        .to_string()
    }

    fn yield_ended(call: &str, input: Value) -> String {
        let receipt =
            crate::subagents::worker::output_contract::yield_receipt(call, input).unwrap();
        json!({"type":"tool_execution_end", "toolName":"yield", "toolCallId":call,
            "isError":false, "result":{"content":[], "details":receipt}})
        .to_string()
    }

    #[test]
    fn yield_protocol_uses_successful_correlated_end_events_and_keeps_failed_calls_pending() {
        let mut reply = yielding_reply();
        let mut output = String::new();
        for call in ["one", "two"] {
            assert!(
                reply
                    .ingest(&yield_started(call), true, &mut output)
                    .unwrap()
                    .is_none()
            );
        }
        let update = json!({"type":"tool_execution_update", "toolName":"yield", "toolCallId":"one",
            "partialResult":{"details":{"schema":"pi.subagent.worker.yield.v1",
                "requestId":"warm-2", "key":1, "data":"preview-canary", "toolCallId":"one"}}});
        assert!(
            reply
                .ingest(&update.to_string(), true, &mut output)
                .unwrap()
                .is_none()
        );
        let result = reply
            .ingest(
                &yield_ended(
                    "one",
                    json!({
                        "requestId":"warm-2", "key":1, "data":null
                    }),
                ),
                true,
                &mut output,
            )
            .unwrap();
        assert_eq!(result, Some((1, BatchAnswer::Data(Value::Null))));
        // Validation alone cannot release the key before the native owner
        // confirms its cancellation-linearized acceptance.
        assert_eq!(reply.yields.as_ref().unwrap().remaining, vec![1, 2]);
        reply.accept_yield(1).unwrap();
        let failed = json!({"type":"tool_execution_end", "toolName":"yield", "toolCallId":"two",
            "isError":true, "result":{"content":[], "isError":true}});
        assert!(
            reply
                .ingest(&failed.to_string(), true, &mut output)
                .unwrap()
                .is_none()
        );
        assert_eq!(reply.yields.as_ref().unwrap().remaining, vec![2]);
        reply
            .ingest(
                &ended(r#"{"items":[{"key":2,"data":false}]}"#),
                true,
                &mut output,
            )
            .unwrap();
        assert!(reply.complete);
        assert!(!output.contains("preview-canary"));
    }

    #[test]
    fn yield_protocol_rejects_stale_foreign_duplicate_and_unstarted_receipts() {
        for input in [
            json!({"requestId":"old-request", "key":1, "data":true}),
            json!({"requestId":"warm-2", "key":3, "data":true}),
        ] {
            let mut reply = yielding_reply();
            reply
                .ingest(&yield_started("one"), true, &mut String::new())
                .unwrap();
            assert!(
                reply
                    .ingest(&yield_ended("one", input), true, &mut String::new())
                    .is_err()
            );
        }
        let mut reply = yielding_reply();
        let receipt = yield_ended("one", json!({"requestId":"warm-2", "key":1, "data":true}));
        assert!(reply.ingest(&receipt, true, &mut String::new()).is_err());

        let mut reply = yielding_reply();
        reply
            .ingest(&yield_started("one"), true, &mut String::new())
            .unwrap();
        reply.ingest(&receipt, true, &mut String::new()).unwrap();
        reply.accept_yield(1).unwrap();
        reply
            .ingest(&yield_started("another"), true, &mut String::new())
            .unwrap();
        assert!(
            reply
                .ingest(
                    &yield_ended(
                        "another",
                        json!({
                            "requestId":"warm-2", "key":1, "data":"replacement-canary"
                        })
                    ),
                    true,
                    &mut String::new()
                )
                .is_err()
        );
        assert_eq!(reply.yields.as_ref().unwrap().remaining, vec![2]);

        let mut reply = yielding_reply();
        reply.started = false;
        assert!(
            reply
                .ingest(&yield_started("one"), true, &mut String::new())
                .is_err()
        );
        let mut reply = yielding_reply();
        reply
            .ingest(&yield_started("one"), true, &mut String::new())
            .unwrap();
        assert!(
            reply
                .ingest(&ended(r#"{"items":[]}"#), true, &mut String::new())
                .is_err()
        );
    }

    #[test]
    fn yield_protocol_rejects_duplicate_json_fields_and_conflicting_result_error_flags() {
        let mut reply = yielding_reply();
        reply
            .ingest(&yield_started("one"), true, &mut String::new())
            .unwrap();
        let duplicate = r#"{"type":"tool_execution_end","toolName":"yield","toolCallId":"one","isError":false,"result":{"content":[],"details":{"schema":"pi.subagent.worker.yield.v1","requestId":"warm-2","key":1,"data":{"n":1,"n":2},"toolCallId":"one"}}}"#;
        assert!(reply.ingest(duplicate, true, &mut String::new()).is_err());
        let mut reply = yielding_reply();
        reply
            .ingest(&yield_started("one"), true, &mut String::new())
            .unwrap();
        let mut conflicting: Value = serde_json::from_str(&yield_ended(
            "one",
            json!({
                "requestId":"warm-2", "key":1, "data":true
            }),
        ))
        .unwrap();
        conflicting["result"]["isError"] = json!(true);
        assert!(
            reply
                .ingest(&conflicting.to_string(), true, &mut String::new())
                .is_err()
        );
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
            let mut system = sysinfo::System::new().expect("initialize sysinfo");
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
        let mut system = sysinfo::System::new().expect("initialize sysinfo");
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
