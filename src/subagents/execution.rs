//! Native subprocess execution and acceptance of a delegated result.
//!
//! Process exit, protocol completion, typed-output acceptance and worktree
//! writeback are separate gates. No isolated edit reaches the parent before
//! all required gates pass. Dropping a pending run kills its process tree and
//! settles the hub lease; rejected worktrees remain available for inspection.
//! One request deadline covers launch, execution, validation and retries. It
//! gates mutation dispatch, but cannot interrupt synchronous Git/filesystem
//! calls or arbitrary host callbacks already running.

use super::deadline::Deadline;
use super::{
    AgentDefinition, SchemaMode, SubagentResult, SubagentStatus, SubagentTask, UpdateCallback,
    append_bounded_line, child_args, child_depth, compile_output_schema, corrective_retry_task,
    emit_progress, protocol, validate_child_output,
};
use crate::agent_cx::AgentCx;
use crate::agent_hub::ChildKind;
use crate::worktree_iso::{IsoApplyMode, IsoHandle, IsoOutcome};
use serde_json::Value;
use std::collections::BTreeMap;
#[cfg(not(unix))]
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
#[cfg(any(not(unix), test))]
use std::sync::mpsc::{self, Receiver};
#[cfg(any(not(unix), test))]
use std::thread::{self, JoinHandle};
use std::time::Duration;
#[cfg(any(not(unix), test))]
use std::time::Instant;

#[cfg(unix)]
mod pipes;
mod ownership;
use ownership::HubLease;

const DRAIN_BATCH: usize = 32;
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const CANCELLED: &str = "Parent cancellation propagated to child process.";

pub(super) struct ChildRunner {
    cwd: PathBuf,
    global_dir: PathBuf,
    child_binary: PathBuf,
    role_model_spec: Option<String>,
    hub_kind: ChildKind,
    deadline: Deadline,
}

impl ChildRunner {
    pub(super) const fn new(
        cwd: PathBuf,
        global_dir: PathBuf,
        child_binary: PathBuf,
        role_model_spec: Option<String>,
        hub_kind: ChildKind,
        deadline: Deadline,
    ) -> Self {
        Self {
            cwd,
            global_dir,
            child_binary,
            role_model_spec,
            hub_kind,
            deadline,
        }
    }

    /// At most one fresh corrective run. The first attempt's isolated edits
    /// are retained, never applied as input to that retry. The original
    /// deadline is retained too, including time spent waiting in the queue.
    #[allow(clippy::too_many_lines)] // Keep acceptance, retry and disposition together.
    pub(super) async fn run_one(
        &self,
        agents: &BTreeMap<String, AgentDefinition>,
        task: SubagentTask,
        step: Option<usize>,
        on_update: Option<UpdateCallback>,
    ) -> SubagentResult {
        let Some(agent) = agents.get(&task.agent) else {
            return SubagentResult::unknown(task, step);
        };
        if let Err(error) = self.deadline.check() {
            return SubagentResult::failed(agent, task, step, error.to_string());
        }
        let schema = task
            .output_schema
            .clone()
            .or_else(|| agent.output_schema.clone());
        if let Some(schema) = &schema
            && let Err(error) = compile_output_schema(schema)
        {
            return SubagentResult::failed(
                agent,
                task,
                step,
                format!("Invalid outputSchema: {error}"),
            );
        }
        let owner = AgentCx::for_current_or_request();
        let update = on_update.as_ref();
        let mut attempt = self
            .run_child_process(agent, task.clone(), step, update, schema.as_ref(), &owner)
            .await;
        if attempt.result.is_error || schema.is_none() {
            return attempt.finish(&owner, true, update);
        }
        let schema = schema.as_ref().expect("schema checked above");
        attempt.result.schema_retries = Some(0);
        match validate_child_output(&attempt.result.output, schema) {
            Ok(data) => {
                attempt.result.data = Some(data);
                attempt.result.schema_valid = Some(true);
                return attempt.finish(&owner, true, update);
            }
            Err(errors) => {
                attempt.result.schema_valid = Some(false);
                attempt.result.validation_errors = Some(errors);
            }
        }

        let errors = attempt.result.validation_errors.clone().unwrap_or_default();
        attempt.result.fail("Child output failed schema validation; preserving this attempt before one corrective retry.".to_string());
        let mut previous = attempt.finish(&owner, false, update);
        // A hub kill during validation/settlement is not a schema failure to
        // repair by launching a replacement child behind the operator's back.
        if matches!(previous.status, SubagentStatus::Cancelled) {
            return previous;
        }
        // Finishing an attempt can perform a snapshot or invoke a host callback.
        // Never spend a new launch after either cancellation or budget expiry.
        if owner.checkpoint().is_err() {
            cancel(&mut previous, CANCELLED);
            emit_progress(update, &previous);
            return previous;
        }
        if let Err(error) = self.deadline.check() {
            previous.fail(error.to_string());
            emit_progress(update, &previous);
            return previous;
        }
        let corrective = SubagentTask {
            task: corrective_retry_task(&task.task, &errors),
            ..task.clone()
        };
        let mut retry = self
            .run_child_process(agent, corrective, step, update, Some(schema), &owner)
            .await;
        retry.result.schema_retries = Some(1);
        // Keep the public assignment stable; corrective prompt text is a
        // transport detail, not a replacement for the user's original task.
        retry.result.task.clone_from(&task.task);
        if let Some(iso) = previous.iso {
            retry.result.preserved_worktrees.push(iso);
        }
        if retry.result.is_error {
            retry.result.schema_valid = Some(false);
            retry.result.validation_errors = Some(errors);
            return retry.finish(&owner, false, update);
        }
        match validate_child_output(&retry.result.output, schema) {
            Ok(data) => {
                retry.result.data = Some(data);
                retry.result.schema_valid = Some(true);
            }
            Err(errors) => {
                retry.result.schema_valid = Some(false);
                retry.result.validation_errors = Some(errors);
                if task.schema_mode == SchemaMode::Strict {
                    retry.result.fail("Child output failed schema validation after the corrective retry (schemaMode: strict).".to_string());
                }
            }
        }
        // Permissive mode permits returning an invalid answer with a warning,
        // not installing edits that failed the requested acceptance contract.
        let accepted = retry.result.schema_valid == Some(true);
        retry.finish(&owner, accepted, update)
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    async fn run_child_process(
        &self,
        agent: &AgentDefinition,
        task: SubagentTask,
        step: Option<usize>,
        update: Option<&UpdateCallback>,
        schema: Option<&Value>,
        owner: &AgentCx,
    ) -> Attempt {
        let cwd = task.cwd.as_ref().map_or_else(
            || self.cwd.clone(),
            |path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    self.cwd.join(path)
                }
            },
        );
        let args = child_args(agent, &task.task, self.role_model_spec.as_deref(), schema);
        let policy = isolation_policy(&task);
        let mut attempt = Attempt::new(
            SubagentResult::starting(agent, task, step, &self.child_binary, &cwd, &args),
            self.deadline,
        );
        if !check_budget(owner, self.deadline, &mut attempt.result) {
            return attempt;
        }
        let capabilities = owner.capabilities();
        if !capabilities.io || !capabilities.spawn || !capabilities.time {
            attempt.result.fail(
                "PI_SUBAGENT_PERMISSION: child execution requires I/O, spawn and timer capabilities"
                    .to_string(),
            );
            return attempt;
        }
        let (isolated, mode) = match policy {
            Ok(policy) => policy,
            Err(error) => {
                attempt.result.fail(error);
                return attempt;
            }
        };
        if !cwd.is_dir() {
            attempt.result.fail(format!(
                "Working directory does not exist: {}",
                cwd.display()
            ));
            return attempt;
        }
        // Tracking is a launch prerequisite, not optional telemetry. A failed
        // registration must never leave an unsteerable/uncontrollable child.
        let hub_entry = match attempt.hub.register(&agent.name, &attempt.result.task, self.hub_kind) {
            Ok(entry) => entry,
            Err(error) => {
                attempt.result.fail(error.to_string());
                return attempt;
            }
        };
        attempt.result.hub_id = Some(hub_entry.id.clone());
        emit_progress(update, &attempt.result);
        if !check_budget(owner, self.deadline, &mut attempt.result) {
            return attempt;
        }
        if isolated {
            match crate::worktree_iso::isolate(&cwd, &attempt.result.task) {
                Ok(handle) => {
                    attempt.result.cwd.clone_from(&handle.path);
                    attempt.isolation = Some((handle, mode));
                }
                Err(error) => {
                    attempt.result.fail(error.to_string());
                    return attempt;
                }
            }
        }
        if !check_budget(owner, self.deadline, &mut attempt.result) {
            return attempt;
        }
        let mut command = Command::new(&self.child_binary);
        if let Err(error) = self.deadline.configure_child(&mut command) {
            attempt.result.fail(error.to_string());
            return attempt;
        }
        command
            .args(&args)
            .current_dir(&attempt.result.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PI_CODING_AGENT_DIR", &self.global_dir)
            .env("PI_SUBAGENT_PARENT_PID", std::process::id().to_string())
            .env("PI_SUBAGENT_DEPTH", child_depth().to_string())
            .env("PI_SUBAGENT_STEER_FILE", &hub_entry.steer_path)
            .env("PI_SUBAGENT_RUN_ID", &hub_entry.id);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        // Check after callbacks and setup, immediately before dispatch.
        if !check_budget(owner, self.deadline, &mut attempt.result) {
            return attempt;
        }
        let child = match owner.process().spawn_checked(&mut command) {
            Ok(child) => child,
            Err(error) => {
                attempt.result.fail(format!(
                    "Failed to launch {}: {error}",
                    self.child_binary.display()
                ));
                return attempt;
            }
        };
        // Guard ownership precedes platform attachment, which may itself fail
        // or unwind. Every successfully spawned child already has a reaper.
        let mut child = ChildProcessGuard::new(child);
        attempt.result.pid = Some(child.id());
        if !crate::tools::attach_child_job_discipline(child.child.as_ref().expect("owned child")) {
            attempt.result.fail(
                "PI_SUBAGENT_CONTAINMENT: failed to attach child process cleanup discipline"
                    .to_string(),
            );
            return attempt;
        }
        // Close the registration-to-spawn race. A kill observed after OS spawn
        // still owns a reaper and must not be announced as a new running task.
        if !check_budget(owner, self.deadline, &mut attempt.result) {
            return attempt;
        }
        if !attempt.hub.mark_running(child.id(), &mut attempt.result) {
            return attempt;
        }
        emit_progress(update, &attempt.result);
        let Some(stdout) = child.child.as_mut().and_then(|child| child.stdout.take()) else {
            attempt
                .result
                .fail("Child stdout was not piped.".to_string());
            return attempt;
        };
        let Some(stderr) = child.child.as_mut().and_then(|child| child.stderr.take()) else {
            attempt
                .result
                .fail("Child stderr was not piped.".to_string());
            return attempt;
        };
        // Unix pipes are owned directly by this future. No blocking reader
        // can survive a dropped turn or an escaped descendant retaining EOF.
        #[cfg(unix)]
        let mut pipes = match pipes::ChildPipes::new(stdout, stderr) {
            Ok(pipes) => pipes,
            Err(error) => {
                attempt
                    .result
                    .fail(format!("PI_SUBAGENT_PIPE_SETUP: {error}"));
                return attempt;
            }
        };
        // Preserve the existing non-Unix transport until an equivalent owned
        // overlapped-I/O implementation is available there.
        #[cfg(not(unix))]
        let (rx, stdout, stderr) = {
            let (tx, rx) = mpsc::sync_channel(protocol::PIPE_QUEUE_CAPACITY);
            let stdout = spawn_pipe_reader(stdout, PipeKind::Stdout, tx.clone());
            let stderr = spawn_pipe_reader(stderr, PipeKind::Stderr, tx);
            (rx, stdout, stderr)
        };
        let mut protocol = protocol::ChildProtocol::default();
        loop {
            if !check_budget(owner, self.deadline, &mut attempt.result) {
                child.terminate();
                break;
            }
            #[cfg(unix)]
            pipes.drain(&mut protocol, &mut attempt.result, update);
            #[cfg(not(unix))]
            drain_child_frames(&rx, &mut protocol, &mut attempt.result, update);
            if !check_budget(owner, self.deadline, &mut attempt.result) {
                // Invalid frames and expired budgets must stop the producer,
                // including one that continues writing after agent_end.
                child.terminate();
                break;
            }
            match child.child.as_mut().expect("owned child").try_wait() {
                Ok(Some(status)) => {
                    attempt.result.exit_code = status.code();
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    attempt
                        .result
                        .fail(format!("Failed while waiting for child: {error}"));
                    child.terminate();
                    break;
                }
            }
            poll_pause(owner).await;
        }
        // No descendant should keep writing or hold the pipes open after its
        // root exits. Cleanup gets a separate bounded drain, not a new work budget.
        child.stop_descendants();
        #[cfg(unix)]
        pipes
            .finish(
                &mut protocol,
                &mut attempt.result,
                update,
                owner,
                self.deadline,
            )
            .await;
        #[cfg(not(unix))]
        drain_until_reader_exit(
            rx,
            &mut protocol,
            &mut attempt.result,
            update,
            stdout,
            stderr,
            owner,
            self.deadline,
        )
        .await;
        if check_budget(owner, self.deadline, &mut attempt.result) {
            if attempt.result.exit_code != Some(0) {
                attempt.result.fail(format!(
                    "Child exited with code {}.",
                    attempt.result.exit_code.unwrap_or(-1)
                ));
            } else if let Err(error) = protocol.finish() {
                attempt.result.fail(error.to_string());
            } else {
                attempt.result.status = SubagentStatus::Completed;
            }
        }
        child.disarm();
        attempt
    }
}

fn isolation_policy(task: &SubagentTask) -> Result<(bool, IsoApplyMode), String> {
    let isolated = match task
        .isolation
        .as_deref()
        .unwrap_or("none")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "none" => false,
        "worktree" => true,
        _ => return Err("PI_SUBAGENT_ISOLATION: isolation must be none or worktree".to_string()),
    };
    let mode = IsoApplyMode::parse(task.iso_apply.as_deref()).map_err(|error| error.to_string())?;
    Ok((isolated, mode))
}

fn cancel(result: &mut SubagentResult, message: &str) {
    result.status = SubagentStatus::Cancelled;
    result.error = Some(message.to_string());
    result.is_error = true;
}

/// Preserve an existing failure, and distinguish an execution timeout from a
/// user's cancellation. Neither authorizes accepting or applying partial work.
fn check_budget(owner: &AgentCx, deadline: Deadline, result: &mut SubagentResult) -> bool {
    if owner.checkpoint().is_err() {
        cancel(result, CANCELLED);
        return false;
    }
    if !ownership::checkpoint(result) {
        return false;
    }
    if !result.is_error
        && let Err(error) = deadline.check()
    {
        result.fail(error.to_string());
    }
    !result.is_error
}

struct Attempt {
    result: SubagentResult,
    isolation: Option<(IsoHandle, IsoApplyMode)>,
    hub: HubLease,
    deadline: Deadline,
}

impl Attempt {
    const fn new(result: SubagentResult, deadline: Deadline) -> Self {
        Self {
            result,
            isolation: None,
            hub: HubLease::empty(),
            deadline,
        }
    }

    fn finish(
        mut self,
        owner: &AgentCx,
        accepted: bool,
        update: Option<&UpdateCallback>,
    ) -> SubagentResult {
        check_budget(owner, self.deadline, &mut self.result);
        let accepted = accepted
            && !self.result.is_error
            && matches!(self.result.status, SubagentStatus::Completed);
        if let Some((handle, requested)) = self.isolation.take() {
            // Both apply and explicit drop require an accepted result. A
            // failure keeps the evidence; no failed attempt is auto-deleted.
            let mode = if accepted {
                requested
            } else {
                IsoApplyMode::Keep
            };
            let mut outcome = IsoOutcome {
                schema: crate::worktree_iso::ISO_SCHEMA.to_string(),
                worktree_path: handle.path.display().to_string(),
                branch: handle.branch.clone(),
                diff_stat: String::new(),
                patch: String::new(),
                conflicted_files: Vec::new(),
                apply_mode: mode.as_str().to_string(),
                applied: false,
            };
            match crate::worktree_iso::collect_diff(&handle) {
                Ok((patch, stat)) => {
                    outcome.patch = patch;
                    outcome.diff_stat = stat;
                    // Recheck after snapshot collection, immediately before
                    // mutation dispatch. Once apply begins it is not rolled
                    // back or misreported merely because the clock advances.
                    let in_budget = check_budget(owner, self.deadline, &mut self.result);
                    if !in_budget {
                        outcome.apply_mode = "keep".to_string();
                    } else if mode == IsoApplyMode::Apply {
                        match crate::worktree_iso::apply_to_parent(&handle, &outcome.patch) {
                            Ok(()) => {
                                outcome.applied = true;
                                if let Err(error) = crate::worktree_iso::drop_worktree(&handle) {
                                    self.result.fail(format!("Edits were applied, but isolated worktree cleanup failed: {error}"));
                                }
                            }
                            Err(error) => {
                                outcome.conflicted_files =
                                    error.to_string().lines().map(str::to_string).collect();
                                self.result.fail(error.to_string());
                            }
                        }
                    } else if mode == IsoApplyMode::Drop
                        && let Err(error) = crate::worktree_iso::drop_worktree(&handle)
                    {
                        self.result
                            .fail(format!("Isolated worktree cleanup failed: {error}"));
                    }
                }
                Err(error) => {
                    outcome.apply_mode = "keep".to_string();
                    if !self.result.is_error {
                        self.result.fail(format!(
                            "Failed to collect isolated diff; worktree preserved: {error}"
                        ));
                    }
                }
            }
            self.result.iso = Some(outcome);
        }
        self.hub.settle(&self.result);
        emit_progress(update, &self.result);
        self.result
    }
}

struct ChildProcessGuard {
    child: Option<std::process::Child>,
    descendants_stopped: bool,
}

impl ChildProcessGuard {
    const fn new(child: std::process::Child) -> Self {
        Self {
            child: Some(child),
            descendants_stopped: false,
        }
    }
    fn id(&self) -> u32 {
        self.child.as_ref().map_or(0, std::process::Child::id)
    }
    fn stop_descendants(&mut self) {
        if self.descendants_stopped {
            return;
        }
        self.descendants_stopped = true;
        let pid = self.id();
        if pid == 0 {
            return;
        }
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(pid)
            && let Some(group) = rustix::process::Pid::from_raw(pid)
        {
            // This group was created by CommandExt::process_group(0).
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        #[cfg(not(unix))]
        crate::tools::kill_process_tree(Some(pid));
    }
    fn terminate(&mut self) {
        self.stop_descendants();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    fn disarm(&mut self) {
        let _ = self.child.take();
    }
}

impl Drop for ChildProcessGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Clone, Copy)]
enum PipeKind {
    Stdout,
    Stderr,
}

enum PipeFrame {
    Data(PipeKind, String),
    Error(&'static str),
}

#[cfg(not(unix))]
fn spawn_pipe_reader<R: Read + Send + 'static>(
    pipe: R,
    kind: PipeKind,
    sender: mpsc::SyncSender<PipeFrame>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        loop {
            let bytes = match protocol::read_frame(&mut reader) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => break,
                Err(error) => {
                    let _ = sender.send(PipeFrame::Error(error));
                    break;
                }
            };
            let line = match kind {
                PipeKind::Stderr => String::from_utf8_lossy(&bytes).into_owned(),
                PipeKind::Stdout => {
                    let Ok(line) = String::from_utf8(bytes) else {
                        let _ = sender.send(PipeFrame::Error(
                            "PI_SUBAGENT_PROTOCOL: child stdout is not UTF-8",
                        ));
                        break;
                    };
                    line
                }
            };
            if sender.send(PipeFrame::Data(kind, line)).is_err() {
                break;
            }
        }
    })
}

#[cfg(any(not(unix), test))]
fn drain_child_frames(
    receiver: &Receiver<PipeFrame>,
    protocol: &mut protocol::ChildProtocol,
    result: &mut SubagentResult,
    update: Option<&UpdateCallback>,
) {
    // A continuously producing child cannot starve cancellation or exit checks.
    for _ in 0..DRAIN_BATCH {
        let Ok(frame) = receiver.try_recv() else {
            break;
        };
        apply_child_frame(frame, protocol, result, update);
    }
}

/// One protocol acceptance path for both transports. In particular, a final
/// answer never turns subsequent malformed stdout into an ignored diagnostic.
fn apply_child_frame(
    frame: PipeFrame,
    protocol: &mut protocol::ChildProtocol,
    result: &mut SubagentResult,
    update: Option<&UpdateCallback>,
) {
    match frame {
        PipeFrame::Error(error) if !result.is_error => result.fail(error.to_string()),
        PipeFrame::Data(PipeKind::Stderr, line) => {
            append_bounded_line(&mut result.stderr, &line);
        }
        PipeFrame::Data(PipeKind::Stdout, line)
            if !result.is_error && ownership::checkpoint(result) =>
        {
            match protocol.ingest(&line, &mut result.output) {
                Ok(changed) => {
                    if let Some(id) = &result.hub_id
                        && let Ok(mut registry) = crate::agent_hub::registry().lock()
                    {
                        registry.append_transcript(id, &line);
                    }
                    if changed {
                        emit_progress(update, result);
                    }
                }
                Err(error) => result.fail(error.to_string()),
            }
        }
        _ => {}
    }
}

async fn poll_pause(owner: &AgentCx) {
    owner.time().sleep(Duration::from_millis(10)).await;
}

#[cfg(any(not(unix), test))]
#[allow(clippy::too_many_arguments)]
async fn drain_until_reader_exit(
    receiver: Receiver<PipeFrame>,
    protocol: &mut protocol::ChildProtocol,
    result: &mut SubagentResult,
    update: Option<&UpdateCallback>,
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
    owner: &AgentCx,
    work_deadline: Deadline,
) {
    let deadline = Instant::now() + PIPE_DRAIN_TIMEOUT;
    loop {
        check_budget(owner, work_deadline, result);
        drain_child_frames(&receiver, protocol, result, update);
        if stdout.is_finished() && stderr.is_finished() {
            // Neither thread can produce another frame after this barrier.
            // There can still be a full queue, not merely one drain batch.
            for _ in 0..protocol::PIPE_QUEUE_CAPACITY.div_ceil(DRAIN_BATCH) {
                check_budget(owner, work_deadline, result);
                drain_child_frames(&receiver, protocol, result, update);
            }
            let stdout_ok = stdout.join().is_ok();
            let stderr_ok = stderr.join().is_ok();
            if (!stdout_ok || !stderr_ok) && !result.is_error {
                result.fail(protocol::PIPE_ERROR.to_string());
            }
            return;
        }
        if owner.checkpoint().is_err() {
            cancel(result, CANCELLED);
        }
        if Instant::now() >= deadline {
            if !result.is_error {
                result.fail(
                    "PI_SUBAGENT_PIPE_TIMEOUT: child pipes did not close after process termination"
                        .to_string(),
                );
            }
            return;
        }
        poll_pause(owner).await;
    }
}

#[cfg(test)]
mod settled_queue_tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn drain_fixture(trailing_failure: bool) -> (SubagentResult, protocol::ChildProtocol) {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        let agent = super::super::tan_agent_definition();
        let task: SubagentTask =
            serde_json::from_value(json!({"agent":"tan","task":"fixture"})).unwrap();
        let mut result =
            SubagentResult::starting(&agent, task, None, Path::new("pi"), Path::new("."), &[]);
        let mut protocol = protocol::ChildProtocol::default();
        let (sender, receiver) = mpsc::sync_channel(protocol::PIPE_QUEUE_CAPACITY);
        let completion = json!({"type":"agent_end","messages":[{
            "role":"assistant","stopReason":"stop","content":[{"type":"text","text":"final answer"}]
        }]})
        .to_string();
        for index in 0..protocol::PIPE_QUEUE_CAPACITY {
            let line = if index == 0 && trailing_failure {
                completion.clone()
            } else if index + 1 == protocol::PIPE_QUEUE_CAPACITY {
                if trailing_failure {
                    "not-json".to_string()
                } else {
                    completion.clone()
                }
            } else {
                json!({"type":"usage","tokens":index}).to_string()
            };
            sender
                .send(PipeFrame::Data(PipeKind::Stdout, line))
                .unwrap();
        }
        drop(sender);
        let stdout = thread::spawn(|| {});
        let stderr = thread::spawn(|| {});
        let wait_until = Instant::now() + Duration::from_secs(2);
        while !stdout.is_finished() || !stderr.is_finished() {
            assert!(
                Instant::now() < wait_until,
                "fixture readers did not settle"
            );
            thread::yield_now();
        }
        runtime.block_on(drain_until_reader_exit(
            receiver,
            &mut protocol,
            &mut result,
            None,
            stdout,
            stderr,
            &owner,
            deadline,
        ));
        (result, protocol)
    }

    #[test]
    fn settled_readers_do_not_drop_a_completion_behind_multiple_batches() {
        let (result, protocol) = drain_fixture(false);
        assert!(!result.is_error, "{:?}", result.error);
        assert_eq!(result.output, "final answer");
        assert!(protocol.finish().is_ok());
    }

    #[test]
    fn settled_readers_do_not_hide_a_failure_after_an_earlier_completion() {
        let (result, protocol) = drain_fixture(true);
        assert!(result.is_error);
        assert!(protocol.finish().is_err());
    }
}
