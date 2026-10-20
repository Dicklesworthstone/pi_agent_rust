//! Session-owned, reusable RPC children. A carrier job owns the process even
//! while idle; stable worker identities and numbered turns are separate from
//! that job's lifetime. No Agent borrow or guest interpreter enters this state.

use super::{
    AgentScope, ChildRunner, Deadline, Error, MAX_SUBAGENT_DEPTH, Result, SubagentTool, Value,
    background, current_subagent_depth, execution, workspace,
};
use crate::agent_cx::AgentCx;
use crate::model::{ContentBlock, TextContent};
use crate::text_completion::AuxiliaryPrivacySnapshot;
use crate::tools::ToolOutput;
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const SCHEMA: &str = "pi.subagent.worker.v1";
const MAX_WORKERS: usize = 32;
const MAX_TURNS: u64 = 32;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_TURN_RESULT_BYTES: usize = 128 * 1024;
// Reserving space for static terminal errors keeps the complete per-worker
// retention under one MiB, including cancelled queued turns.
const MAX_WORKER_RESULT_BYTES: usize = 1024 * 1024;
const ERROR_RESERVE_BYTES: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const STOPPED: &str = "PI_SUBAGENT_WORKER_STOPPED: worker stopped before this turn completed; prior side effects may remain";
const WITHHELD: &str = "PI_SUBAGENT_WORKER_RESULT_WITHHELD: result exceeded its retention budget or could not be privacy-screened";

pub(super) type Shared = Arc<Mutex<WorkerState>>;

pub(super) struct PendingTurn {
    pub(super) id: u64,
    pub(super) task: String,
}

struct Turn {
    status: &'static str,
    result: Option<ToolOutput>,
    privacy: Option<AuxiliaryPrivacySnapshot>,
}

pub(super) struct WorkerState {
    owner: String,
    id: String,
    job_id: String,
    agent: String,
    deadline: Deadline,
    acknowledged: bool,
    started: bool,
    closing: bool,
    closed: bool,
    failed: bool,
    latest: u64,
    queue: VecDeque<PendingTurn>,
    turns: BTreeMap<u64, Turn>,
    result_bytes: usize,
}

fn registry() -> &'static Mutex<VecDeque<Shared>> {
    static WORKERS: OnceLock<Mutex<VecDeque<Shared>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn invalid(message: &str) -> Error {
    Error::tool("subagent", format!("PI_SUBAGENT_WORKER_INPUT: {message}"))
}

fn stopped() -> Error {
    Error::tool("subagent", STOPPED)
}

fn error_output(message: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(message))],
        details: Some(json!({"schema": "pi.subagent.worker.error.v1"})),
        is_error: true,
    }
}

pub(super) fn requested(input: &Value) -> Result<bool> {
    match input.get("operation") {
        None => Ok(false),
        Some(Value::String(operation))
            if matches!(
                operation.as_str(),
                "worker_start" | "worker_send" | "worker_status" | "worker_wait" | "worker_close"
            ) =>
        {
            Ok(true)
        }
        Some(_) => Err(invalid("unknown worker operation")),
    }
}

fn validate_fields(input: &Value, allowed: &[&str]) -> Result<()> {
    let fields = input
        .as_object()
        .ok_or_else(|| invalid("expected an object"))?;
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid("operation contains an unsupported field"));
    }
    Ok(())
}

fn required_text<'a>(input: &'a Value, key: &str, max_bytes: usize) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| {
            !value.trim().is_empty() && !value.contains('\0') && value.len() <= max_bytes
        })
        .ok_or_else(|| {
            invalid(&format!(
                "{key} must be nonempty text of at most {max_bytes} bytes without NUL"
            ))
        })
}

fn optional_integer(input: &Value, key: &str, minimum: u64, maximum: u64) -> Result<Option<u64>> {
    input
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .filter(|value| (minimum..=maximum).contains(value))
                .ok_or_else(|| {
                    invalid(&format!(
                        "{key} must be an integer from {minimum} to {maximum}"
                    ))
                })
        })
        .transpose()
}

fn check_owner(owner: &AgentCx) -> Result<()> {
    owner.checkpoint().map_err(|_| stopped())?;
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.spawn || !capabilities.time {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_PERMISSION: worker operations require I/O, spawn and timer capabilities",
        ));
    }
    Ok(())
}

fn register(state: Shared) -> Result<()> {
    let mut workers = registry().lock().map_err(|_| stopped())?;
    if workers.len() == MAX_WORKERS {
        // Never evict a live process. Closed handles expire oldest-first;
        // retaining all 32 result histories costs at most 32 MiB.
        let oldest_closed = workers
            .iter()
            .position(|worker| worker.lock().is_ok_and(|state| state.closed))
            .ok_or_else(|| {
                Error::tool(
                    "subagent",
                    "PI_SUBAGENT_WORKER_CAPACITY: retained worker capacity exhausted",
                )
            })?;
        workers.remove(oldest_closed);
    }
    workers.push_back(state);
    Ok(())
}

fn lookup(owner: &str, id: &str) -> Result<Shared> {
    let workers = registry().lock().map_err(|_| stopped())?;
    workers
        .iter()
        .find_map(|worker| {
            let state = worker.lock().ok()?;
            (state.id == id && state.owner == owner).then(|| Arc::clone(worker))
        })
        .ok_or_else(|| {
            Error::tool(
                "subagent",
                "PI_SUBAGENT_WORKER_UNKNOWN: worker is unavailable in this session",
            )
        })
}

fn identity(state: &Shared) -> Result<(String, String)> {
    let state = state.lock().map_err(|_| stopped())?;
    Ok((state.owner.clone(), state.job_id.clone()))
}

/// This guard also covers a never-polled future rejected by jobs admission.
/// The inner RPC actor drops its process and pipes before the outer guard.
struct FinishGuard(Shared);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        finish(&self.0, None);
    }
}

async fn start(
    tool: &SubagentTool,
    input: Value,
    privacy: AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    validate_fields(
        &input,
        &[
            "operation",
            "agent",
            "task",
            "cwd",
            "scope",
            "timeoutSeconds",
            "isolation",
        ],
    )?;
    if !cfg!(unix) {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_WORKER_UNSUPPORTED: reusable RPC workers currently require Unix nonblocking pipes",
        ));
    }
    let task = required_text(&input, "task", MAX_PROMPT_BYTES)?.to_string();
    let name = required_text(&input, "agent", 128)?.to_string();
    if name == "seance" {
        return Err(invalid(
            "historical consultation cannot become a reusable worker",
        ));
    }
    workspace::validate_request(&input)?;
    if input
        .get("isolation")
        .is_some_and(|value| value.as_str() != Some("none"))
    {
        return Err(invalid(
            "workers currently support isolation none; use ordinary subagent tasks for worktrees",
        ));
    }
    let timeout = optional_integer(&input, "timeoutSeconds", 1, 86_400)?;
    let scope: AgentScope = input
        .get("scope")
        .map_or(Ok(AgentScope::default()), |value| {
            serde_json::from_value(value.clone())
                .map_err(|_| invalid("scope must be user, project or both"))
        })?;
    let owner = AgentCx::for_current_or_request();
    check_owner(&owner)?;
    if current_subagent_depth() >= MAX_SUBAGENT_DEPTH {
        return Err(invalid("maximum nested subagent depth reached"));
    }
    let deadline = Deadline::for_request(tool.timeout, timeout)?;
    let owner_id = background::resolve_session(&tool.job_session_scope, &owner, deadline).await?;
    let admission = crate::jobs::admit_owned_task(&owner_id)?;
    let parent_model = tool.capture_parent_model();
    let mut definitions = tool.discover(scope)?;
    let agent = definitions
        .remove(&name)
        .ok_or_else(|| invalid("unknown agent definition"))?;
    if agent.output_schema.is_some() {
        return Err(invalid(
            "schema-bearing agents currently require ordinary subagent tasks",
        ));
    }
    let selected_cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .map_or_else(|| tool.cwd.clone(), |path| tool.cwd.join(path));
    let cwd = std::fs::canonicalize(&selected_cwd)
        .map_err(|error| Error::tool("subagent", format!("PI_SUBAGENT_WORKER_CWD: {error}")))?;
    if !cwd.is_dir() {
        return Err(invalid("cwd must name an existing directory"));
    }
    if background::resolve_session(&tool.job_session_scope, &owner, deadline).await? != owner_id {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_SESSION_CHANGED: worker owner changed during admission",
        ));
    }
    check_owner(&owner)?;
    deadline
        .check()
        .map_err(|error| Error::tool("subagent", error))?;
    let id = format!("worker-{}", uuid::Uuid::new_v4().simple());
    let state = Arc::new(Mutex::new(WorkerState {
        owner: owner_id.clone(),
        id: id.clone(),
        job_id: String::new(),
        agent: name,
        deadline,
        acknowledged: false,
        started: false,
        closing: false,
        closed: false,
        failed: false,
        latest: 1,
        queue: VecDeque::from([PendingTurn { id: 1, task }]),
        turns: BTreeMap::from([(
            1,
            Turn {
                status: "queued",
                result: None,
                privacy: Some(privacy.clone()),
            },
        )]),
        result_bytes: 0,
    }));
    register(Arc::clone(&state))?;
    let runner = ChildRunner::new(
        cwd,
        tool.global_dir.clone(),
        tool.child_binary.clone(),
        tool.role_model_spec.clone(),
        crate::agent_hub::ChildKind::Subagent,
        deadline,
    )
    .with_parent_model(parent_model);
    let cleanup = FinishGuard(Arc::clone(&state));
    let actor_state = Arc::clone(&state);
    let artifacts = tool.global_dir.join("tool-output-artifacts");
    let job = crate::jobs::spawn_owned_task(
        admission,
        "reusable subagent worker",
        owner.clone(),
        deadline.remaining(),
        Some(&artifacts),
        privacy.clone(),
        async move {
            let _cleanup = cleanup;
            execution::worker::run(runner, agent, actor_state).await
        },
    );
    match job {
        Ok(job) => {
            state.lock().map_err(|_| stopped())?.job_id = job.id;
            // A privacy refusal must not admit an invisible task. The actor
            // may establish RPC readiness, but cannot dequeue until this
            // complete acknowledgment has passed projection and identity checks.
            let acknowledgment = async {
                let output = snapshot(&state, Some(1), &privacy)?;
                if background::resolve_session(&tool.job_session_scope, &owner, deadline).await?
                    != owner_id
                {
                    return Err(Error::tool(
                        "subagent",
                        "PI_SUBAGENT_SESSION_CHANGED: worker owner changed before acknowledgment",
                    ));
                }
                check_owner(&owner)?;
                let (_, job_id) = identity(&state)?;
                crate::jobs::with_owned_task_progress(&owner_id, &job_id, || {
                    let mut state = state.lock().map_err(|_| stopped())?;
                    if state.closed || state.closing || state.deadline.check().is_err() {
                        return Err(stopped());
                    }
                    state.acknowledged = true;
                    Ok(((), None))
                })?;
                Ok(output)
            }
            .await;
            match acknowledgment {
                Ok(output) => Ok(output),
                Err(error) => {
                    close(&state).await?;
                    Err(error)
                }
            }
        }
        Err(error) => {
            // The rejected task has already dropped its owned cleanup guard.
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|worker| !Arc::ptr_eq(worker, &state));
            Err(error)
        }
    }
}

pub(super) async fn execute(
    tool: &SubagentTool,
    input: Value,
    privacy: AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    let operation = input
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("operation is required"))?;
    if operation == "worker_start" {
        return start(tool, input, privacy).await;
    }
    let allowed: &[&str] = match operation {
        "worker_send" => &["operation", "workerId", "task"],
        "worker_status" => &["operation", "workerId", "turnId"],
        "worker_wait" => &["operation", "workerId", "turnId", "timeoutMs"],
        "worker_close" => &["operation", "workerId"],
        _ => return Err(invalid("unknown worker operation")),
    };
    validate_fields(&input, allowed)?;
    let id = required_text(&input, "workerId", 128)?;
    let selected_turn = optional_integer(&input, "turnId", 1, MAX_TURNS)?;
    let timeout_ms = optional_integer(&input, "timeoutMs", 0, 600_000)?.unwrap_or(30_000);
    let prompt = if operation == "worker_send" {
        Some(required_text(&input, "task", MAX_PROMPT_BYTES)?.to_string())
    } else {
        None
    };
    let owner = AgentCx::for_current_or_request();
    check_owner(&owner)?;
    let budget = Deadline::for_request(Some(Duration::from_millis(timeout_ms + 30_000)), None)?;
    let owner_id = background::resolve_session(&tool.job_session_scope, &owner, budget).await?;
    let state = lookup(&owner_id, id)?;
    let (_, job_id) = identity(&state)?;
    if let Some(task) = prompt {
        // Prepare a small acknowledgment before mutating the queue. Concurrent
        // SDK sends may win the next number; retry preparation, never duplicate
        // the assignment or return an unprotected/missing accepted handle.
        for _ in 0..MAX_TURNS {
            let (turn_id, value) = {
                let state = state.lock().map_err(|_| stopped())?;
                if state.latest >= MAX_TURNS {
                    return Err(Error::tool(
                        "subagent",
                        "PI_SUBAGENT_WORKER_TURN_LIMIT: worker reached its 32-turn limit; close it and start a new worker",
                    ));
                }
                let next = state.latest + 1;
                (
                    next,
                    json!({
                        "schema": SCHEMA, "workerId": state.id, "jobId": state.job_id, "agent": state.agent,
                        "status": if state.started { "running" } else { "starting" }, "turnId": next,
                        "turn": {"id": next, "status": "queued", "result": null},
                        "admittedTurns": next, "queuedTurns": state.queue.len() + 1,
                        "remainingMs": u64::try_from(state.deadline.remaining().as_millis()).unwrap_or(u64::MAX),
                    }),
                )
            };
            let acknowledgment = projected_snapshot(value, &privacy)?;
            check_owner(&owner)?;
            if background::resolve_session(&tool.job_session_scope, &owner, budget).await?
                != owner_id
            {
                return Err(Error::tool(
                    "subagent",
                    "PI_SUBAGENT_SESSION_CHANGED: worker owner changed before send",
                ));
            }
            let admitted = crate::jobs::with_owned_task_progress(&owner_id, &job_id, || {
                let mut state = state.lock().map_err(|_| stopped())?;
                if state.closed || state.closing || state.deadline.check().is_err() {
                    return Err(stopped());
                }
                if state.latest + 1 != turn_id {
                    return Ok((false, None));
                }
                state.latest = turn_id;
                state.turns.insert(
                    turn_id,
                    Turn {
                        status: "queued",
                        result: None,
                        privacy: Some(privacy.clone()),
                    },
                );
                state.queue.push_back(PendingTurn {
                    id: turn_id,
                    task: task.clone(),
                }); // ubs:ignore[rust.collections.clone-in-loop] -- Only the winning bounded admission copies this task into its owned queue; retries have no side effects.
                Ok((true, None))
            })?;
            if admitted {
                return Ok(acknowledgment);
            }
        }
        return Err(invalid(
            "concurrent sends exhausted this worker's turn budget",
        ));
    }
    if operation == "worker_close" {
        if background::resolve_session(&tool.job_session_scope, &owner, budget).await? != owner_id {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_SESSION_CHANGED: worker owner changed before close",
            ));
        }
        check_owner(&owner)?;
        close(&state).await?;
    }
    // Capture latest once: a concurrent send cannot retarget an existing wait.
    let turn_id = selected_turn.unwrap_or(state.lock().map_err(|_| stopped())?.latest);
    let started = Instant::now();
    loop {
        check_owner(&owner)?;
        if background::resolve_session(&tool.job_session_scope, &owner, budget).await? != owner_id {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_SESSION_CHANGED: worker observation owner changed",
            ));
        }
        let output = snapshot(&state, Some(turn_id), &privacy)?;
        let pending = output
            .details
            .as_ref()
            .and_then(|details| details.pointer("/turn/status"))
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "queued" | "running"));
        if operation != "worker_wait"
            || !pending
            || started.elapsed() >= Duration::from_millis(timeout_ms)
        {
            return Ok(output);
        }
        let time = owner.time();
        time.sleep(
            POLL_INTERVAL.min(Duration::from_millis(timeout_ms).saturating_sub(started.elapsed())),
        )
        .await;
    }
}

async fn close(state: &Shared) -> Result<()> {
    let (owner_id, job_id) = {
        let mut state = state.lock().map_err(|_| stopped())?;
        state.closing = true;
        if state.closed {
            return Ok(());
        }
        (state.owner.clone(), state.job_id.clone())
    };
    // Never hold a worker lock while entering jobs. The progress fence always
    // acquires jobs before worker. Actor closure proves reap even while the
    // carrier is still screening its final artifact and reads as Running.
    if let Err(error) = crate::jobs::cancel_async(&owner_id, &job_id).await {
        if state.lock().map_err(|_| stopped())?.closed {
            return Ok(());
        }
        let settled = crate::jobs::wait_async(&owner_id, &job_id, Duration::from_secs(10)).await?;
        if settled.status == "running" {
            return Err(error);
        }
    }
    Ok(())
}

fn snapshot(
    state: &Shared,
    selected: Option<u64>,
    privacy: &AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    let value = {
        let state = state.lock().map_err(|_| stopped())?;
        let turn_id = selected.unwrap_or(state.latest);
        let turn = state
            .turns
            .get(&turn_id)
            .ok_or_else(|| invalid("turnId was not admitted by this worker"))?;
        let status = if state.closed {
            if state.failed { "failed" } else { "closed" }
        } else if !state.started {
            "starting"
        } else if state
            .turns
            .values()
            .any(|turn| matches!(turn.status, "queued" | "running"))
        {
            "running"
        } else {
            "idle"
        };
        json!({
            "schema": SCHEMA, "workerId": state.id, "jobId": state.job_id, "agent": state.agent,
            "status": status, "turnId": turn_id,
            "turn": {"id": turn_id, "status": turn.status, "result": turn.result},
            "admittedTurns": state.latest, "queuedTurns": state.queue.len(),
            "remainingMs": u64::try_from(state.deadline.remaining().as_millis()).unwrap_or(u64::MAX),
        })
    };
    projected_snapshot(value, privacy)
}

fn projected_snapshot(value: Value, privacy: &AuxiliaryPrivacySnapshot) -> Result<ToolOutput> {
    // Re-screen stored results under the current invocation's policy/vault.
    // A later discovery may be stricter than the snapshot that admitted a turn.
    let details = privacy.project_json(&value)?;
    for path in [
        "/schema",
        "/workerId",
        "/jobId",
        "/agent",
        "/status",
        "/turnId",
        "/turn/id",
        "/turn/status",
    ] {
        if value.pointer(path) != details.pointer(path) {
            return Err(Error::tool(
                "subagent",
                "PI_SUBAGENT_WORKER_PRIVACY: privacy policy withheld worker protocol identity",
            ));
        }
    }
    let status = details
        .pointer("/turn/status")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    let mut text = format!(
        "Reusable worker turn is {status}. Use worker_wait with its workerId and turnId for the native result; worker_close retires the process and all queued work."
    );
    if let Some(blocks) = details
        .pointer("/turn/result/content")
        .and_then(Value::as_array)
    {
        let answer = blocks
            .iter()
            .filter_map(|block| {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    block.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !answer.is_empty() {
            text.push_str("\n\n");
            text.push_str(&super::truncated_field(&answer, 16 * 1024));
        }
    }
    Ok(ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: false,
    })
}

pub(super) fn next_turn(shared: &Shared) -> Option<PendingTurn> {
    let mut state = shared.lock().ok()?;
    if state.closed
        || state.closing
        || !state.acknowledged
        || state.job_id.is_empty()
        || state.deadline.check().is_err()
    {
        return None;
    }
    state.started = true;
    let turn = state.queue.pop_front()?;
    state.turns.get_mut(&turn.id)?.status = "running";
    Some(turn)
}

pub(super) fn should_close(shared: &Shared) -> bool {
    shared.lock().map_or(true, |state| {
        state.closed || state.closing || state.deadline.check().is_err()
    })
}

fn protected_result(result: Result<ToolOutput>, privacy: &AuxiliaryPrivacySnapshot) -> ToolOutput {
    let output = result.unwrap_or_else(|error| error_output(&error.to_string()));
    let projected = (|| {
        let bytes = serde_json::to_vec(&output).ok()?;
        if bytes.len() > MAX_TURN_RESULT_BYTES {
            return None;
        }
        let value = serde_json::from_slice::<Value>(&bytes).ok()?;
        let value = privacy.project_json(&value).ok()?;
        if serde_json::to_vec(&value).ok()?.len() > MAX_TURN_RESULT_BYTES {
            return None;
        }
        serde_json::from_value(value).ok()
    })();
    projected.unwrap_or_else(|| error_output(WITHHELD))
}

pub(super) fn complete_turn(shared: &Shared, id: u64, result: Result<ToolOutput>) {
    let admission = {
        let mut state = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(turn) = state.turns.get_mut(&id) else {
            return;
        };
        let Some(privacy) = turn.privacy.take() else {
            return;
        };
        (state.owner.clone(), state.job_id.clone(), privacy)
    };
    let mut output = protected_result(result, &admission.2);
    let size = serde_json::to_vec(&output).map_or(MAX_TURN_RESULT_BYTES, |bytes| bytes.len());
    let text = output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let summary = super::truncated_field(&text, 16 * 1024);
    let withheld = error_output(WITHHELD);
    let withheld_bytes =
        serde_json::to_vec(&withheld).map_or(ERROR_RESERVE_BYTES, |bytes| bytes.len());
    // Serialization and privacy scanning happen outside both lifecycle locks.
    // The short commit closure linearizes acceptance and the completion notice
    // against jobs cancellation and session shutdown.
    let accepted = crate::jobs::with_owned_task_progress(&admission.0, &admission.1, || {
        let mut state = shared.lock().map_err(|_| stopped())?;
        if state.closed || state.closing || state.deadline.check().is_err() {
            return Err(stopped());
        }
        let over_budget =
            state.result_bytes.saturating_add(size) > MAX_WORKER_RESULT_BYTES - ERROR_RESERVE_BYTES;
        if over_budget {
            output = withheld;
        }
        let bytes = if over_budget { withheld_bytes } else { size };
        let status = if output.is_error {
            "failed"
        } else {
            "completed"
        };
        let Some(turn) = state.turns.get_mut(&id) else {
            return Err(stopped());
        };
        if turn.status != "running" {
            return Err(stopped());
        }
        turn.status = status;
        turn.result = Some(output);
        state.result_bytes = state.result_bytes.saturating_add(bytes);
        let notice = format!(
            "[reusable worker {} turn {id} {status}]\n{}",
            state.id,
            if over_budget { WITHHELD } else { &summary }
        );
        Ok(((), Some(notice)))
    });
    if accepted.is_err() {
        // Lost publication authority must stop the actor before another queued
        // task can execute. Actual closed state still waits for process reap.
        shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closing = true;
    }
}

/// Called only after the actor's process and pipes have been retired. The
/// outer future guard supplies cancellation settlement when the actor is
/// dropped instead of returning. First terminal state wins.
pub(super) fn finish(shared: &Shared, error: Option<String>) {
    let mut state = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.closed {
        return;
    }
    state.closed = true;
    state.failed = error.is_some() && !state.closing;
    let status = if state.failed { "failed" } else { "cancelled" };
    state.queue.clear();
    for turn in state.turns.values_mut() {
        turn.privacy = None;
        if matches!(turn.status, "queued" | "running") {
            turn.status = status;
            turn.result = Some(error_output(STOPPED));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_completion::AuxiliaryPrivacy;

    fn private_projection(secret: &str) -> AuxiliaryPrivacySnapshot {
        let mut vault = crate::secrets::SecretVault::default();
        let pattern = regex::Regex::new(secret).expect("controlled privacy fixture"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- This literal regex seeds the remembered-secret regression.
        crate::secrets::obfuscate(secret, &mut vault, &[pattern]);
        AuxiliaryPrivacySnapshot::with_vault(AuxiliaryPrivacy::from_settings(None), &vault)
    }

    #[test]
    fn malformed_controls_never_downgrade_to_ordinary_delegation() {
        for operation in [Value::Null, json!(true), json!("worker_restart"), json!({})] {
            assert!(requested(&json!({"operation":operation})).is_err()); // ubs:ignore[rust.panic.assert-macros] -- Explicit malformed control must not launch an unrelated one-shot task.
        }
        assert!(
            validate_fields(
                &json!({"operation":"worker_send", "workerId":"w", "task":"t", "cwd":"elsewhere"}),
                &["operation", "workerId", "task"]
            )
            .is_err()
        ); // ubs:ignore[rust.panic.assert-macros] -- A send cannot silently broaden its frozen workspace.
        for value in [
            Value::Null,
            json!(false),
            json!(1.5),
            json!("1"),
            json!(0),
            json!(33),
        ] {
            assert!(optional_integer(&json!({"turnId":value}), "turnId", 1, MAX_TURNS).is_err()); // ubs:ignore[rust.panic.assert-macros] -- Turn identities are bounded exact integers, never coerced.
        }
    }

    #[test]
    fn turn_storage_screens_remembered_values_in_text_and_native_details() {
        let secret = "opaque-worker-secret-from-previous-cell";
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(secret))],
            details: Some(json!({"answer":secret})),
            is_error: false,
        };
        let protected = protected_result(Ok(output), &private_projection(secret));
        let encoded = serde_json::to_string(&protected).expect("projected result"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Serialization is required to inspect every retained field.
        assert!(!encoded.contains(secret)); // ubs:ignore[rust.panic.assert-macros] -- Neither text nor nested native details may retain a known secret.
        assert!(encoded.contains("<pi-secret:redacted>")); // ubs:ignore[rust.panic.assert-macros] -- Stored results must use irreversible redaction.
        assert!(!protected.is_error); // ubs:ignore[rust.panic.assert-macros] -- Successful redaction preserves the successful task result.
    }

    #[test]
    fn oversized_turn_result_withholds_whole_payload() {
        let privacy = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None));
        let result = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "private-payload-marker".repeat(MAX_TURN_RESULT_BYTES / 8),
            ))],
            details: None,
            is_error: false,
        };
        let protected = protected_result(Ok(result), &privacy);
        let encoded = serde_json::to_string(&protected).expect("withheld result"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Inspect the complete replacement before any retention.
        assert!(protected.is_error); // ubs:ignore[rust.panic.assert-macros] -- A withheld answer must never appear successful.
        assert!(encoded.contains("PI_SUBAGENT_WORKER_RESULT_WITHHELD")); // ubs:ignore[rust.panic.assert-macros] -- The named failure lets callers distinguish output-budget failure.
        assert!(!encoded.contains("private-payload-marker")); // ubs:ignore[rust.panic.assert-macros] -- Oversized results must not leak a raw prefix.
    }

    #[test]
    fn privacy_cannot_turn_a_worker_acknowledgment_into_an_unusable_handle() {
        let worker_id = "worker-private-identity-fixture";
        let value = json!({
            "schema": SCHEMA, "workerId": worker_id, "jobId":"job-fixture", "agent":"task",
            "status":"starting", "turnId":1, "turn":{"id":1, "status":"queued", "result":null},
        });
        let error = projected_snapshot(value, &private_projection(worker_id))
            .expect_err("identity withheld"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- An acknowledgment whose worker ID cannot be disclosed must be refused before task admission.
        assert!(error.to_string().contains("PI_SUBAGENT_WORKER_PRIVACY")); // ubs:ignore[rust.panic.assert-macros] -- Protocol identity must not be silently replaced with a redaction token.
    }

    #[test]
    fn direct_tool_wait_exposes_the_answer_in_model_visible_content() {
        let value = json!({
            "schema": SCHEMA, "workerId":"worker-fixture", "jobId":"job-fixture", "agent":"scout",
            "status":"idle", "turnId":2, "turn":{"id":2, "status":"completed", "result":{
                "content":[{"type":"text", "text":"The retry loop is in src/provider.rs."}], "details":null,
            }},
        });
        let privacy = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None));
        let output = projected_snapshot(value, &privacy).expect("successful turn observation"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Valid native results must render without eval-specific access to details.
        assert!(output.content.iter().any(|block| matches!(block, ContentBlock::Text(text) if text.text.contains("The retry loop is in src/provider.rs.")))); // ubs:ignore[rust.panic.assert-macros] -- A direct subagent tool caller must receive the completed answer, not only metadata.
    }
}
