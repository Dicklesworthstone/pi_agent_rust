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

pub(super) mod isolation;
pub(super) mod output_contract;
mod pool;
pub(super) mod yield_tool;

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

pub(super) struct PendingBatch {
    pub(super) turns: Vec<PendingTurn>,
    pub(super) keyed: bool,
    pub(super) yield_results: bool,
}

struct Turn {
    status: &'static str,
    result: Option<ToolOutput>,
    privacy: Option<AuxiliaryPrivacySnapshot>,
    dependency: Arc<crate::agent_hub::ResultDependency>,
}

#[allow(clippy::struct_excessive_bools)] // Independent lifecycle flags read under one lock.
pub(super) struct WorkerState {
    owner: String,
    hub: crate::agent_hub::SharedHub,
    id: String,
    job_id: String,
    hub_id: Option<String>,
    agent: String,
    deadline: Deadline,
    emit_notices: bool,
    acknowledged: bool,
    started: bool,
    closing: bool,
    closed: bool,
    failed: bool,
    latest: u64,
    max_batch_size: usize,
    yield_results: bool,
    // Logical items may settle before their shared RPC turn reaches idle.
    // Keep that physical conversation leased until its correlated barrier.
    active_batch: bool,
    queue: VecDeque<PendingTurn>,
    turns: BTreeMap<u64, Turn>,
    result_bytes: usize,
    // Latest active context observed at the correlated post-compaction idle
    // barrier. Provider usage can describe discarded history, so it is never
    // used for scheduling retained workers.
    context_tokens: Option<u64>,
    context_window: Option<u64>,
    // Recovery identity survives process retirement, including a cancelled turn
    // that never produced a complete cumulative patch.
    workspace: Option<Value>,
    workspace_roots: Option<Value>,
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
    for field in ["maxBatchSize", "yieldResults"] {
        if input.get(field).is_some()
            && input.get("operation").and_then(Value::as_str) != Some("pool_start")
        {
            return Err(invalid(&format!("{field} is only accepted by pool_start")));
        }
    }
    match input.get("operation") {
        None => Ok(false),
        Some(Value::String(operation))
            if matches!(
                operation.as_str(),
                "worker_start"
                    | "worker_send"
                    | "worker_status"
                    | "worker_wait"
                    | "worker_close"
                    | "pool_start"
                    | "pool_push"
                    | "pool_finish"
                    | "pool_status"
                    | "pool_result"
                    | "pool_wait"
                    | "pool_drain"
                    | "pool_exchange"
                    | "pool_close"
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

/// Keep an unpublished worker out of retained history if its caller is dropped
/// while native admission or the final ownership check is pending.
struct Registration(Option<Shared>);

impl Registration {
    fn commit(mut self) {
        self.0 = None;
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(state) = self.0.take() {
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|worker| !Arc::ptr_eq(worker, &state));
        }
    }
}

fn register(state: Shared) -> Result<Registration> {
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
    workers.push_back(Arc::clone(&state));
    drop(workers);
    Ok(Registration(Some(state)))
}

fn lookup(owner: &str, id: &str) -> Result<Shared> {
    let hub = crate::agent_hub::current_registry();
    let workers = registry().lock().map_err(|_| stopped())?;
    workers
        .iter()
        .find_map(|worker| {
            let state = worker.lock().ok()?;
            (state.id == id && state.owner == owner && Arc::ptr_eq(&state.hub, &hub))
                .then(|| Arc::clone(worker))
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

/// Called after native hub admission and before the actor launches its process.
/// Already-admitted waits observe this binding through their own turn tokens.
pub(super) fn bind_hub(state: &Shared, id: &str) -> Result<()> {
    let hub = crate::agent_hub::current_registry();
    let mut state = state.lock().map_err(|_| stopped())?;
    if !Arc::ptr_eq(&state.hub, &hub) || state.hub_id.is_some() {
        return Err(stopped());
    }
    for turn in state.turns.values() {
        turn.dependency.bind(id);
    }
    state.hub_id = Some(id.to_string());
    Ok(())
}

/// Retain the exact workspace identity before spawning its child. Even a
/// startup failure or interrupted turn must leave its files discoverable.
pub(super) fn bind_workspace(state: &Shared, descriptor: Value) -> Result<()> {
    let mut state = state.lock().map_err(|_| stopped())?;
    if state.workspace.is_some() {
        return Err(stopped());
    }
    state.workspace = Some(descriptor);
    Ok(())
}

pub(super) fn bind_workspace_roots(state: &Shared, descriptor: Value) -> Result<()> {
    let mut state = state.lock().map_err(|_| stopped())?;
    if state.workspace_roots.is_some() {
        return Err(stopped());
    }
    state.workspace_roots = Some(descriptor);
    Ok(())
}

fn hold_results(
    results: Arc<crate::agent_hub::ResultDependencies>,
    acknowledged: Vec<u64>,
    all: bool,
    remaining: Duration,
) -> Result<Option<Arc<crate::agent_hub::DependencyHold>>> {
    if crate::jobs::current_native_task().is_some() {
        // An internal background wait does not prevent the root parent from
        // answering. Do not invent a foreground dependency for that caller.
        return Ok(None);
    }
    let registry = crate::agent_hub::current_registry();
    let mut hub = registry
        .lock()
        .map_err(|_| Error::tool("subagent", "PI_SUBAGENT_HUB: registry is poisoned"))?;
    hub.hold_turn_results(results, acknowledged, all, remaining)
        .map(Some)
}

/// This guard also covers a never-polled future rejected by jobs admission.
/// The inner RPC actor drops its process and pipes before the outer guard.
struct FinishGuard(Shared);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        let hub = Arc::clone(
            &self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .hub,
        );
        let _hub_scope = crate::agent_hub::enter_registry(hub);
        finish(&self.0, None);
    }
}

#[allow(clippy::too_many_lines)] // One ordered admission from input checks to acknowledgment.
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
            "model",
            "cwd",
            "scope",
            "timeoutSeconds",
            "isolation",
            "isoApply",
            "outputSchema",
            "schemaMode",
        ],
    )?;
    if !cfg!(any(unix, windows)) {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_WORKER_UNSUPPORTED: reusable RPC workers require Unix or Windows nonblocking pipes",
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
    isolation::validate(&input)?;
    let timeout = optional_integer(&input, "timeoutSeconds", 1, 86_400)?;
    let scope: AgentScope = input.get("scope").map_or_else(
        || Ok(AgentScope::default()),
        |value| {
            serde_json::from_value(value.clone())
                .map_err(|_| invalid("scope must be user, project or both"))
        },
    )?;
    let owner = AgentCx::for_current_or_request();
    check_owner(&owner)?;
    if current_subagent_depth() >= MAX_SUBAGENT_DEPTH {
        return Err(invalid("maximum nested subagent depth reached"));
    }
    let deadline = tool.request_deadline(timeout)?;
    let job_deadline = Instant::now() + deadline.remaining();
    let owner_id =
        background::resolve_invocation_session(&tool.job_session_scope, &owner, deadline).await?;
    let admission = crate::jobs::admit_owned_task(&owner_id)?.with_deadline(job_deadline);
    let parent_model = tool.capture_parent_model();
    let cost_budget = tool.cost_scope.current();
    let workspace_roots = workspace::CapturedRoots::capture(&tool.workspace, privacy.clone())?;
    let mut definitions = tool.discover(scope)?;
    let mut agent = definitions
        .remove(&name)
        .ok_or_else(|| invalid("unknown agent definition"))?;
    let schema = input.get("outputSchema").or(agent.output_schema.as_ref());
    if input
        .get("schemaMode")
        .is_some_and(|mode| mode.as_str() != Some("strict"))
        || (input.get("schemaMode").is_some() && schema.is_none())
    {
        return Err(invalid(
            "retained output contracts require a schema and strict mode; no corrective retries",
        ));
    }
    output_contract::OutputContract::preflight(schema, &privacy)?;
    if let Some(schema) = input.get("outputSchema") {
        agent.output_schema = Some(schema.clone());
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
    let (model_selection, fallback_policy) =
        super::model_selection::prepare_worker(tool, &input, &agent, &cwd, deadline)?;
    check_owner(&owner)?;
    deadline
        .check()
        .map_err(|error| Error::tool("subagent", error))?;
    let isolation = isolation::Capture::prepare(&input, &cwd, &privacy)?;
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
        hub: crate::agent_hub::current_registry(),
        id: id.clone(),
        job_id: String::new(),
        hub_id: None,
        agent: name,
        deadline,
        emit_notices: true,
        acknowledged: false,
        started: false,
        closing: false,
        closed: false,
        failed: false,
        latest: 1,
        max_batch_size: 1,
        yield_results: false,
        active_batch: false,
        queue: VecDeque::from([PendingTurn { id: 1, task }]),
        turns: BTreeMap::from([(
            1,
            Turn {
                status: "queued",
                result: None,
                privacy: Some(privacy.clone()),
                dependency: crate::agent_hub::ResultDependency::new(None),
            },
        )]),
        result_bytes: 0,
        context_tokens: None,
        context_window: None,
        workspace: None,
        workspace_roots: None,
    }));
    let registration = register(Arc::clone(&state))?;
    let runner = ChildRunner::new(
        cwd,
        tool.global_dir.clone(),
        tool.child_binary.clone(),
        tool.role_model_spec.clone(),
        crate::agent_hub::ChildKind::Subagent,
        deadline,
    )
    .with_parent_model(parent_model)
    .with_cost_budget(cost_budget)
    .with_workspace_roots(workspace_roots)
    .with_worker_model_selection(model_selection)
    .with_worker_fallback_policy(fallback_policy)
    .with_worker_isolation(isolation);
    let cleanup = FinishGuard(Arc::clone(&state));
    let actor_state = Arc::clone(&state);
    let artifacts = tool.global_dir.join("tool-output-artifacts");
    let launch = crate::jobs::spawn_owned_task_async(
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
    )
    .await?;
    let job = launch.snapshot()?;
    state.lock().map_err(|_| stopped())?.job_id = job.id;
    // Native work remains unpolled until the complete acknowledgment passes
    // projection and identity checks. Dropping this future also drops the
    // armed launch and registration, without starting an invisible RPC child.
    let output = snapshot(&state, Some(1), &privacy)?;
    if background::resolve_session(&tool.job_session_scope, &owner, deadline).await? != owner_id {
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
        drop(state);
        Ok(((), None))
    })?;
    launch.publish()?;
    registration.commit();
    Ok(output)
}

#[allow(clippy::too_many_lines)] // One dispatcher: send admission, close and the wait loop.
pub(super) async fn execute(
    tool: &SubagentTool,
    input: Value,
    privacy: AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    let operation = input
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("operation is required"))?;
    if operation.starts_with("pool_") {
        return pool::execute(tool, input, privacy).await;
    }
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
    let budget = tool.cap_deadline(Deadline::for_request(
        Some(Duration::from_millis(timeout_ms + 30_000)),
        None,
    )?);
    let owner_id =
        background::resolve_invocation_session(&tool.job_session_scope, &owner, budget).await?;
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
                let dependency = crate::agent_hub::ResultDependency::new(state.hub_id.as_deref());
                state.turns.insert(
                    turn_id,
                    Turn {
                        status: "queued",
                        result: None,
                        privacy: Some(privacy.clone()),
                        dependency,
                    },
                );
                state.queue.push_back(PendingTurn {
                    id: turn_id,
                    task: task.clone(),
                }); // ubs:ignore[rust.collections.clone-in-loop] -- Only the winning bounded admission copies this task into its owned queue; retries have no side effects.
                drop(state);
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
    let _waiting = if operation == "worker_wait" && timeout_ms > 0 {
        let results = state
            .lock()
            .map_err(|_| stopped())?
            .turns
            .get(&turn_id)
            .map(|turn| Arc::clone(&turn.dependency))
            .into_iter()
            .collect();
        hold_results(
            crate::agent_hub::ResultDependencies::new(results),
            Vec::new(),
            true,
            Duration::from_millis(timeout_ms),
        )?
    } else {
        None
    };
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
    let hub = Arc::clone(&state.lock().map_err(|_| stopped())?.hub);
    crate::agent_hub::with_registry(hub, close_in_registry(state)).await
}

async fn close_in_registry(state: &Shared) -> Result<()> {
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
        } else if state.active_batch
            || state
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
            "yieldResults": state.yield_results,
            "contextTokens": state.context_tokens, "contextWindow": state.context_window,
            "workspace": state.workspace,
            "workspaceRoots": state.workspace_roots,
            "remainingMs": u64::try_from(state.deadline.remaining().as_millis()).unwrap_or(u64::MAX),
        })
    };
    projected_snapshot(value, privacy)
}

#[allow(clippy::needless_pass_by_value)] // Callers hand over freshly built snapshots.
fn projected_snapshot(value: Value, privacy: &AuxiliaryPrivacySnapshot) -> Result<ToolOutput> {
    // Re-screen stored results under the current invocation's policy/vault.
    // A later discovery may be stricter than the snapshot that admitted a turn.
    let details = privacy.project_json(&value)?;
    if let Some(result) = value.pointer("/turn/result")
        && (!output_contract::preserves_typed_result(
            result,
            details.pointer("/turn/result").unwrap_or(&Value::Null),
        ) || !isolation::preserves_result(
            result,
            details.pointer("/turn/result").unwrap_or(&Value::Null),
        ))
    {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_WORKER_PRIVACY: privacy policy withheld typed data or a worktree patch",
        ));
    }
    for path in [
        "/schema",
        "/workerId",
        "/jobId",
        "/agent",
        "/status",
        "/turnId",
        "/turn/id",
        "/turn/status",
        "/yieldResults",
        "/contextTokens",
        "/contextWindow",
        "/workspace",
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

/// Select one bounded FIFO group without changing the logical item identities.
/// A grouped worker retains its keyed contract even for a singleton group.
pub(super) fn next_batch(shared: &Shared) -> Option<PendingBatch> {
    let mut state = shared.lock().ok()?;
    if state.closed
        || state.closing
        || state.active_batch
        || !state.acknowledged
        || state.job_id.is_empty()
        || state.deadline.check().is_err()
        || state.turns.values().any(|turn| turn.status == "running")
    {
        return None;
    }
    let mut bytes = 0usize;
    let mut count = 0usize;
    for turn in state.queue.iter().take(state.max_batch_size) {
        if bytes.saturating_add(turn.task.len()) > MAX_PROMPT_BYTES {
            break;
        }
        if state.turns.get(&turn.id)?.status != "queued" {
            return None;
        }
        bytes += turn.task.len();
        count += 1;
    }
    if count == 0 {
        return None;
    }
    let turns: Vec<_> = state.queue.drain(..count).collect();
    for turn in &turns {
        // Every record was checked under this same lock before dequeueing.
        if let Some(record) = state.turns.get_mut(&turn.id) {
            record.status = "running";
        }
    }
    state.started = true;
    state.active_batch = true;
    let keyed = state.max_batch_size > 1;
    let yield_results = state.yield_results;
    drop(state);
    Some(PendingBatch {
        turns,
        keyed,
        yield_results,
    })
}

pub(super) fn yield_results(shared: &Shared) -> bool {
    shared.lock().is_ok_and(|state| state.yield_results)
}

/// Release the physical turn only after the actor observes its correlated idle
/// barrier and checks its owner. Early item publication cannot release it.
pub(super) fn finish_batch(shared: &Shared) -> Result<()> {
    let mut state = shared.lock().map_err(|_| stopped())?;
    if state.closed
        || state.closing
        || !state.active_batch
        || state.deadline.check().is_err()
        || state.turns.values().any(|turn| turn.status == "running")
    {
        return Err(stopped());
    }
    state.active_batch = false;
    Ok(())
}

pub(super) fn should_close(shared: &Shared) -> bool {
    shared.lock().map_or(true, |state| {
        state.closed || state.closing || state.deadline.check().is_err()
    })
}

pub(super) fn observe_idle_context(shared: &Shared, tokens: Option<u64>, window: Option<u64>) {
    if let Ok(mut state) = shared.lock()
        && !state.closed
        && !state.closing
        && state.deadline.check().is_ok()
    {
        state.context_tokens = tokens;
        state.context_window = window;
    }
}

fn protected_result(result: Result<ToolOutput>, privacy: &AuxiliaryPrivacySnapshot) -> ToolOutput {
    let output = result.unwrap_or_else(|error| error_output(&error.to_string()));
    let projected = (|| {
        let bytes = serde_json::to_vec(&output).ok()?;
        if bytes.len() > MAX_TURN_RESULT_BYTES {
            return None;
        }
        let value = serde_json::from_slice::<Value>(&bytes).ok()?;
        let projected = privacy.project_json(&value).ok()?;
        if !output_contract::preserves_typed_result(&value, &projected)
            || !isolation::preserves_result(&value, &projected)
            || serde_json::to_vec(&projected).ok()?.len() > MAX_TURN_RESULT_BYTES
        {
            return None;
        }
        serde_json::from_value(projected).ok()
    })();
    projected.unwrap_or_else(|| error_output(WITHHELD))
}

pub(super) fn complete_turn(
    shared: &Shared,
    id: u64,
    result: Result<ToolOutput>,
    workspace_roots: Option<&workspace::CapturedRoots>,
) -> bool {
    let admission = {
        let mut state = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.closing || !state.active_batch || state.deadline.check().is_err() {
            return false;
        }
        let Some(turn) = state.turns.get_mut(&id) else {
            return false;
        };
        if turn.status != "running" {
            return false;
        }
        let Some(privacy) = turn.privacy.take() else {
            return false;
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
        // Recheck after schema/privacy work, at the same acceptance boundary
        // as cancellation. Root mutations hold only their own short lock and
        // never enter jobs or worker state, so this lock order cannot reverse.
        if let Some(roots) = workspace_roots {
            roots.check()?;
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
        turn.dependency.complete();
        state.result_bytes = state.result_bytes.saturating_add(bytes);
        let notice = state.emit_notices.then(|| {
            format!(
                "[reusable worker {} turn {id} {status}]\n{}",
                state.id,
                if over_budget { WITHHELD } else { &summary }
            )
        });
        drop(state);
        Ok(((), notice))
    });
    if accepted.is_err() {
        // Lost publication authority must stop the actor before another queued
        // task can execute. Actual closed state still waits for process reap.
        shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closing = true;
    }
    accepted.is_ok()
}

/// Called only after the actor's process and pipes have been retired. The
/// outer future guard supplies cancellation settlement when the actor is
/// dropped instead of returning. First terminal state wins.
pub(super) fn finish(shared: &Shared, error: Option<&str>) {
    let mut state = shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.closed {
        return;
    }
    state.closed = true;
    state.active_batch = false;
    state.failed = error.is_some() && !state.closing;
    let status = if state.failed { "failed" } else { "cancelled" };
    state.queue.clear();
    for turn in state.turns.values_mut() {
        turn.dependency.complete();
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

    fn queued_worker(max_batch_size: usize, assignments: &[&str]) -> Shared {
        let privacy = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None));
        let mut queue = VecDeque::new();
        let mut turns = BTreeMap::new();
        for (index, task) in assignments.iter().enumerate() {
            let id = u64::try_from(index + 1).expect("bounded test item id");
            queue.push_back(PendingTurn {
                id,
                task: (*task).to_string(),
            });
            turns.insert(
                id,
                Turn {
                    status: "queued",
                    result: None,
                    privacy: Some(privacy.clone()),
                    dependency: crate::agent_hub::ResultDependency::new(None),
                },
            );
        }
        Arc::new(Mutex::new(WorkerState {
            owner: "batch-owner".to_string(),
            hub: crate::agent_hub::current_registry(),
            id: "batch-worker".to_string(),
            job_id: "batch-job".to_string(),
            hub_id: None,
            agent: "reviewer".to_string(),
            deadline: Deadline::for_request(Some(Duration::from_secs(60)), None)
                .expect("bounded test deadline"),
            emit_notices: false,
            acknowledged: true,
            started: false,
            closing: false,
            closed: false,
            failed: false,
            latest: u64::try_from(assignments.len()).expect("bounded test item count"),
            max_batch_size,
            yield_results: false,
            active_batch: false,
            queue,
            turns,
            result_bytes: 0,
            context_tokens: Some(321),
            context_window: Some(4096),
            workspace: None,
            workspace_roots: None,
        }))
    }

    fn settle_selected(shared: &Shared, batch: &PendingBatch) {
        {
            let mut state = shared.lock().expect("worker fixture");
            for turn in &batch.turns {
                state.turns.get_mut(&turn.id).expect("selected item").status = "completed";
            }
        }
        finish_batch(shared).expect("simulated post-turn idle barrier");
    }

    #[test]
    fn worker_handle_control_requires_the_same_live_hub_even_with_a_matching_session_id() {
        let own = Arc::new(Mutex::new(crate::agent_hub::AgentHubRegistry::default()));
        let foreign = Arc::new(Mutex::new(crate::agent_hub::AgentHubRegistry::default()));
        let owner_id = format!("same-saved-session-{}", uuid::Uuid::new_v4().simple());
        let worker_id = format!("worker-{}", uuid::Uuid::new_v4().simple());
        let (state, _registration) = {
            let _scope = crate::agent_hub::enter_registry(Arc::clone(&own));
            let state = queued_worker(1, &["owned assignment"]);
            {
                let mut worker = state.lock().unwrap();
                worker.owner.clone_from(&owner_id);
                worker.id.clone_from(&worker_id);
            }
            let registration = register(Arc::clone(&state)).unwrap();
            assert!(Arc::ptr_eq(&lookup(&owner_id, &worker_id).unwrap(), &state));
            (state, registration)
        };
        {
            let _scope = crate::agent_hub::enter_registry(Arc::clone(&foreign));
            let error = lookup(&owner_id, &worker_id)
                .err()
                .expect("another live session must not borrow the saved-session identity");
            assert!(error.to_string().contains("PI_SUBAGENT_WORKER_UNKNOWN"));
            assert!(bind_hub(&state, "foreign-child-1").is_err());
            assert!(state.lock().unwrap().hub_id.is_none());
            assert!(!state.lock().unwrap().closing);
        }
        let _scope = crate::agent_hub::enter_registry(own);
        assert!(Arc::ptr_eq(&lookup(&owner_id, &worker_id).unwrap(), &state));
    }

    #[test]
    fn grouped_dispatch_keeps_fifo_identities_privacy_and_dependencies() {
        let shared = queued_worker(2, &["first", "second", "third"]);
        let secrets = ["first-private-group-marker", "second-private-group-marker"];
        {
            let mut state = shared.lock().expect("worker fixture");
            for (id, secret) in [1_u64, 2].into_iter().zip(secrets) {
                state.turns.get_mut(&id).expect("queued item").privacy =
                    Some(private_projection(secret));
            }
        }
        let dependencies: Vec<_> = shared
            .lock()
            .expect("worker fixture")
            .turns
            .values()
            .map(|turn| Arc::clone(&turn.dependency))
            .collect();
        let batch = next_batch(&shared).expect("first group");
        assert!(batch.keyed);
        assert_eq!(
            batch.turns.iter().map(|turn| turn.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            batch
                .turns
                .iter()
                .map(|turn| turn.task.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        {
            let state = shared.lock().expect("worker fixture");
            assert_eq!(state.queue.front().expect("remaining item").id, 3);
            assert_eq!(state.queue.len(), 1);
            assert_eq!(state.latest, 3);
            assert_eq!(state.context_tokens, Some(321));
            assert_eq!(state.context_window, Some(4096));
            for (index, turn) in state.turns.values().enumerate() {
                assert_eq!(turn.status, if index < 2 { "running" } else { "queued" });
                assert!(turn.privacy.is_some());
                assert!(Arc::ptr_eq(&turn.dependency, &dependencies[index]));
                if let Some(secret) = secrets.get(index) {
                    let projected = turn
                        .privacy
                        .as_ref()
                        .expect("captured item privacy")
                        .project_json(&json!({"value":secret}))
                        .expect("privacy projection");
                    assert_ne!(projected["value"], json!(secret));
                }
            }
        }
        // A second dispatcher cannot overlap the group's borrowed conversation.
        assert!(next_batch(&shared).is_none());
        settle_selected(&shared, &batch);
        let singleton = next_batch(&shared).expect("remaining singleton");
        assert!(singleton.keyed);
        assert_eq!(singleton.turns.len(), 1);
        assert_eq!(singleton.turns[0].id, 3);
        assert_eq!(singleton.turns[0].task, "third");
    }

    #[test]
    fn default_dispatch_retains_one_plain_assignment_per_turn() {
        let shared = queued_worker(1, &["first", "second"]);
        let first = next_batch(&shared).expect("first ordinary turn");
        assert!(!first.keyed);
        assert!(!first.yield_results);
        assert_eq!(first.turns.len(), 1);
        assert_eq!(first.turns[0].id, 1);
        settle_selected(&shared, &first);
        let second = next_batch(&shared).expect("second ordinary turn");
        assert!(!second.keyed);
        assert_eq!(second.turns.len(), 1);
        assert_eq!(second.turns[0].id, 2);
        settle_selected(&shared, &second);
        assert!(next_batch(&shared).is_none());
    }

    #[test]
    fn early_results_keep_the_physical_batch_running_until_its_idle_barrier() {
        let shared = queued_worker(2, &["first", "second"]);
        shared.lock().expect("worker fixture").yield_results = true;
        assert!(yield_results(&shared));
        let batch = next_batch(&shared).expect("yield-enabled batch");
        assert!(batch.yield_results && batch.keyed);
        assert!(finish_batch(&shared).is_err());
        {
            let mut state = shared.lock().expect("worker fixture");
            for turn in state.turns.values_mut() {
                turn.status = "completed";
                turn.privacy = None;
            }
        }
        let privacy = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None));
        let observed = snapshot(&shared, Some(1), &privacy)
            .expect("early result observation")
            .details
            .expect("native worker receipt");
        assert_eq!(observed["status"], "running");
        assert_eq!(observed["turn"]["status"], "completed");
        assert_eq!(observed["yieldResults"], true);
        assert!(shared.lock().expect("worker fixture").active_batch);

        // Newly admitted work cannot reuse the conversation while the child
        // is still producing its terminal response or compacting history.
        {
            let mut state = shared.lock().expect("worker fixture");
            state.latest = 3;
            state.queue.push_back(PendingTurn {
                id: 3,
                task: "later".to_string(),
            });
            state.turns.insert(
                3,
                Turn {
                    status: "queued",
                    result: None,
                    privacy: Some(privacy),
                    dependency: crate::agent_hub::ResultDependency::new(None),
                },
            );
        }
        assert!(next_batch(&shared).is_none());
        finish_batch(&shared).expect("correlated idle barrier");
        assert!(finish_batch(&shared).is_err());
        let later = next_batch(&shared).expect("dispatch after idle");
        assert_eq!(later.turns.len(), 1);
        assert_eq!(later.turns[0].id, 3);
        assert!(later.yield_results && later.keyed);
    }

    #[test]
    fn yielded_result_commits_are_immutable_and_obey_owner_cancellation() {
        let _guard = crate::jobs::process_test_guard();
        let artifacts = tempfile::tempdir().expect("owned result fixture artifacts");
        let owner = format!("yield-result-fixture-{}", uuid::Uuid::new_v4().simple());
        let privacy = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None));
        let job = crate::jobs::spawn_owned_task(
            crate::jobs::admit_owned_task(&owner).expect("native task admission"),
            "yield result publication fixture",
            AgentCx::for_request(),
            Duration::from_secs(30),
            Some(artifacts.path()),
            privacy,
            futures::future::pending(),
        )
        .expect("live publication owner");
        let shared = queued_worker(4, &["first", "large", "invalid", "cancelled", "queued"]);
        {
            let mut state = shared.lock().expect("worker fixture");
            state.owner.clone_from(&owner);
            state.job_id.clone_from(&job.id);
            state.yield_results = true;
        }
        next_batch(&shared).expect("active native batch");
        let accepted = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("accepted answer"))],
            details: Some(json!({"value":"first"})),
            is_error: false,
        };
        assert!(complete_turn(&shared, 1, Ok(accepted.clone()), None));
        assert!(!complete_turn(&shared, 1, Err(stopped()), None));
        assert!(!complete_turn(&shared, 5, Err(stopped()), None));
        assert!(!complete_turn(&shared, 99, Err(stopped()), None));
        {
            let state = shared.lock().expect("worker fixture");
            assert!(!state.closing);
            assert!(state.turns[&5].privacy.is_some());
        }
        assert!(complete_turn(
            &shared,
            2,
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(
                    "oversized-yield-marker".repeat(MAX_TURN_RESULT_BYTES / 8),
                ))],
                details: None,
                is_error: false,
            }),
            None,
        ));
        assert!(complete_turn(
            &shared,
            3,
            Err(Error::tool("subagent", "invalid typed answer")),
            None,
        ));
        {
            let state = shared.lock().expect("worker fixture");
            assert_eq!(state.turns[&2].status, "failed");
            assert_eq!(state.turns[&3].status, "failed");
            let withheld =
                serde_json::to_string(&state.turns[&2].result).expect("withheld yield result");
            assert!(withheld.contains(WITHHELD));
            assert!(!withheld.contains("oversized-yield-marker"));
        }
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native cancellation runtime");
        runtime
            .block_on(crate::jobs::cancel_async(&owner, &job.id))
            .expect("cancel publication owner");
        assert!(!complete_turn(&shared, 4, Ok(accepted.clone()), None));
        assert!(finish_batch(&shared).is_err());
        finish(&shared, None);
        let state = shared.lock().expect("worker fixture");
        assert!(state.closed && !state.active_batch);
        assert_eq!(state.turns[&1].status, "completed");
        assert_eq!(
            serde_json::to_value(&state.turns[&1].result).expect("retained accepted answer"),
            serde_json::to_value(Some(accepted)).expect("original accepted answer"),
        );
        assert_eq!(state.turns[&4].status, "cancelled");
        assert_eq!(state.turns[&5].status, "cancelled");
        assert!(state.turns.values().all(|turn| turn.privacy.is_none()));
    }

    #[test]
    fn result_publication_rechecks_root_grants_and_preserves_accepted_prefix() {
        let _guard = crate::jobs::process_test_guard();
        let artifacts = tempfile::tempdir().expect("root lease publication artifacts");
        let delegated = tempfile::tempdir().expect("delegated workspace root");
        let mut workspace = crate::workspace::WorkspaceHandle::single(artifacts.path());
        workspace.add_root(delegated.path());
        let privacy = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None));
        let roots = workspace::CapturedRoots::capture(&workspace, privacy.clone())
            .expect("capture live workspace grants")
            .expect("one delegated grant");
        let owner = format!("yield-roots-fixture-{}", uuid::Uuid::new_v4().simple());
        let job = crate::jobs::spawn_owned_task(
            crate::jobs::admit_owned_task(&owner).expect("native task admission"),
            "yield workspace publication fixture",
            AgentCx::for_request(),
            Duration::from_secs(30),
            Some(artifacts.path()),
            privacy,
            futures::future::pending(),
        )
        .expect("live publication owner");
        let shared = queued_worker(2, &["accepted", "revoked"]);
        {
            let mut state = shared.lock().expect("worker fixture");
            state.owner.clone_from(&owner);
            state.job_id.clone_from(&job.id);
            state.yield_results = true;
        }
        next_batch(&shared).expect("active native batch");
        let output = ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(
                "accepted before revocation",
            ))],
            details: Some(json!({"value":"retained"})),
            is_error: false,
        };
        assert!(complete_turn(&shared, 1, Ok(output.clone()), Some(&roots)));
        assert!(workspace.remove_root(delegated.path()));
        workspace.add_root(delegated.path());
        assert_eq!(workspace.additional_roots().len(), 1);
        assert!(!complete_turn(&shared, 2, Ok(output.clone()), Some(&roots)));
        // This owner is still live. Refusal comes from the old captured grant,
        // even though the same path was granted again before publication.
        crate::jobs::with_owned_task_progress(&owner, &job.id, || Ok(((), None)))
            .expect("owner remains live after root revocation");
        {
            let state = shared.lock().expect("worker fixture");
            assert!(state.closing);
            assert_eq!(state.turns[&1].status, "completed");
            assert_eq!(state.turns[&2].status, "running");
            assert!(state.turns[&2].result.is_none());
            assert_eq!(
                serde_json::to_value(&state.turns[&1].result).expect("accepted prefix"),
                serde_json::to_value(Some(output)).expect("original accepted result"),
            );
        }
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native cancellation runtime");
        runtime
            .block_on(crate::jobs::cancel_async(&owner, &job.id))
            .expect("retire fixture owner");
        finish(&shared, None);
        let state = shared.lock().expect("worker fixture");
        assert!(state.closed && !state.active_batch);
        assert_eq!(state.turns[&1].status, "completed");
        assert_eq!(state.turns[&2].status, "cancelled");
    }

    #[test]
    fn grouped_dispatch_bounds_combined_utf8_bytes_without_skipping_fifo_items() {
        let near_limit = "雪".repeat((MAX_PROMPT_BYTES - 1) / 3);
        let shared = queued_worker(8, &[&near_limit, "ab", "x"]);
        let first = next_batch(&shared).expect("bounded first group");
        assert_eq!(first.turns.len(), 1);
        assert_eq!(first.turns[0].task.len(), MAX_PROMPT_BYTES - 1);
        assert_eq!(shared.lock().expect("worker fixture").queue.len(), 2);
        settle_selected(&shared, &first);
        let tail = next_batch(&shared).expect("remaining group");
        assert_eq!(
            tail.turns.iter().map(|turn| turn.id).collect::<Vec<_>>(),
            vec![2, 3]
        );

        let full = "x".repeat(MAX_PROMPT_BYTES);
        let shared = queued_worker(8, &[&full, "tail"]);
        assert_eq!(next_batch(&shared).expect("full first item").turns.len(), 1);
        assert_eq!(shared.lock().expect("worker fixture").queue.len(), 1);

        let half = "x".repeat(MAX_PROMPT_BYTES / 2);
        let shared = queued_worker(8, &[&half, &half, "tail"]);
        assert_eq!(
            next_batch(&shared)
                .expect("exact byte boundary")
                .turns
                .len(),
            2
        );
        assert_eq!(shared.lock().expect("worker fixture").queue.len(), 1);
    }

    #[test]
    fn grouped_dispatch_obeys_lifecycle_and_validates_the_complete_prefix() {
        for boundary in [
            "unacknowledged",
            "unowned",
            "closing",
            "closed",
            "missing",
            "settled",
        ] {
            let shared = queued_worker(2, &["first", "second"]);
            {
                let mut state = shared.lock().expect("worker fixture");
                match boundary {
                    "unacknowledged" => state.acknowledged = false,
                    "unowned" => state.job_id.clear(),
                    "closing" => state.closing = true,
                    "closed" => state.closed = true,
                    "missing" => {
                        state.turns.remove(&2);
                    }
                    "settled" => {
                        state.turns.get_mut(&2).expect("second item").status = "completed";
                    }
                    _ => unreachable!("controlled fixture boundary"),
                }
            }
            assert!(next_batch(&shared).is_none(), "{boundary}");
            let state = shared.lock().expect("worker fixture");
            assert_eq!(state.queue.len(), 2, "{boundary}");
            assert_eq!(state.turns[&1].status, "queued", "{boundary}");
            assert!(!state.started, "{boundary}");
        }
    }

    fn private_projection(secret: &str) -> AuxiliaryPrivacySnapshot {
        let mut vault = crate::secrets::SecretVault::default();
        let pattern =
            regex::Regex::new(&regex::escape(secret)).expect("controlled privacy fixture"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- This literal regex seeds the remembered-secret regression.
        crate::secrets::obfuscate(secret, &mut vault, &[pattern]);
        AuxiliaryPrivacySnapshot::with_vault(AuxiliaryPrivacy::from_settings(None), &vault)
    }

    #[test]
    fn malformed_controls_never_downgrade_to_ordinary_delegation() {
        for operation in [
            None,
            Some("worker_start"),
            Some("worker_send"),
            Some("pool_push"),
            Some("pool_exchange"),
            Some("pool_status"),
        ] {
            for (field, value) in [("maxBatchSize", json!(2)), ("yieldResults", json!(true))] {
                let mut input = json!({});
                input[field] = value;
                if let Some(operation) = operation {
                    input["operation"] = json!(operation);
                }
                assert!(requested(&input).is_err());
            }
        }
        assert!(requested(&json!({"operation":"pool_start","maxBatchSize":2})).unwrap());
        assert!(requested(&json!({"operation":"pool_start","yieldResults":true})).unwrap());
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
