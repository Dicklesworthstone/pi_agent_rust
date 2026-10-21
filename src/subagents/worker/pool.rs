//! A session-owned pool is one native job, including its idle RPC children,
//! admission queue and ordered result manifest. Guest interpreters retain only
//! identities; no callback or borrowed host authority drives the scheduler.

use super::{
    AgentCx, AgentScope, AuxiliaryPrivacySnapshot, BTreeMap, ChildRunner, ContentBlock, Deadline,
    Error, MAX_PROMPT_BYTES, MAX_SUBAGENT_DEPTH, MAX_TURNS, Mutex, OnceLock, POLL_INTERVAL,
    PendingTurn, Result, Shared, SubagentTool, TextContent, ToolOutput, Turn, Value, VecDeque,
    WorkerState, background, check_owner, current_subagent_depth, execution, optional_integer,
    required_text, validate_fields, workspace,
};
use crate::subagents::AgentDefinition;
use futures::stream::{FuturesUnordered, StreamExt};
use serde_json::json;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SCHEMA: &str = "pi.subagent.pool.v1";
const MAX_POOLS: usize = 16;
const MAX_CHILDREN: u64 = 8;
const MAX_ITEMS: usize = 32;
const SUMMARY_BYTES: usize = 2048;
const MAX_SNAPSHOT_BYTES: usize = 768 * 1024;

type Pool = Arc<Mutex<PoolState>>;
type ActorFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

struct Item {
    worker: usize,
    turn: u64,
}

struct PoolState {
    owner: String,
    id: String,
    job_id: String,
    agent: String,
    deadline: Deadline,
    acknowledged: bool,
    sealed: bool,
    closing: bool,
    closed: bool,
    failed: bool,
    workers: Vec<Shared>,
    items: Vec<Item>,
    // At most one snapshot per accepted batch (plus an empty initial batch).
    // A later discovery must also protect earlier summaries in final delivery.
    privacy: Vec<AuxiliaryPrivacySnapshot>,
}

fn invalid(message: &str) -> Error {
    Error::tool("subagent", format!("PI_SUBAGENT_POOL_INPUT: {message}"))
}

fn stopped() -> Error {
    Error::tool(
        "subagent",
        "PI_SUBAGENT_POOL_STOPPED: pool is sealed, stopped or past its original deadline",
    )
}

fn registry() -> &'static Mutex<VecDeque<Pool>> {
    static POOLS: OnceLock<Mutex<VecDeque<Pool>>> = OnceLock::new();
    POOLS.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn register(pool: Pool) -> Result<()> {
    let mut pools = registry().lock().map_err(|_| stopped())?;
    if pools.len() == MAX_POOLS {
        let retired = pools
            .iter()
            .position(|pool| pool.lock().is_ok_and(|state| state.closed))
            .ok_or_else(|| {
                Error::tool(
                    "subagent",
                    "PI_SUBAGENT_POOL_CAPACITY: retained pool capacity exhausted",
                )
            })?;
        // At most 32 * 128 KiB of item results per pool. Only a reaped pool
        // may be evicted; 16 retained pools therefore bound result storage.
        pools.remove(retired);
    }
    pools.push_back(pool);
    Ok(())
}

fn lookup(owner: &str, id: &str) -> Result<Pool> {
    registry()
        .lock()
        .map_err(|_| stopped())?
        .iter()
        .find_map(|pool| {
            let state = pool.lock().ok()?;
            (state.owner == owner && state.id == id).then(|| Arc::clone(pool))
        })
        .ok_or_else(|| {
            Error::tool(
                "subagent",
                "PI_SUBAGENT_POOL_UNKNOWN: pool is unavailable in this session",
            )
        })
}

fn tasks(input: &Value, allow_empty: bool) -> Result<Vec<String>> {
    let values = match input.get("tasks") {
        None if allow_empty => return Ok(Vec::new()),
        Some(Value::Array(values)) => values,
        _ => return Err(invalid("tasks must be an array of string assignments")),
    };
    if values.len() > MAX_ITEMS || (!allow_empty && values.is_empty()) {
        return Err(invalid(
            "tasks must contain at most 32 assignments; pool_push requires at least one",
        ));
    }
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .filter(|task| {
                    !task.trim().is_empty()
                        && !task.contains('\0')
                        && task.len() <= MAX_PROMPT_BYTES
                })
                .map(str::to_string)
                .ok_or_else(|| {
                    invalid("each assignment must be nonempty text of at most 64 KiB without NUL")
                })
        })
        .collect()
}

fn worker(owner: &str, agent: &str, deadline: Deadline) -> Shared {
    Arc::new(Mutex::new(WorkerState {
        owner: owner.to_string(),
        id: format!("pool-member-{}", uuid::Uuid::new_v4().simple()),
        job_id: String::new(),
        agent: agent.to_string(),
        deadline,
        emit_notices: false,
        acknowledged: false,
        started: false,
        closing: false,
        closed: false,
        failed: false,
        latest: 0,
        queue: VecDeque::new(),
        turns: BTreeMap::new(),
        result_bytes: 0,
    }))
}

#[allow(clippy::too_many_lines)]
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
            "tasks",
            "concurrency",
            "cwd",
            "scope",
            "timeoutSeconds",
            "isolation",
        ],
    )?;
    if !cfg!(unix) {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_POOL_UNSUPPORTED: reusable pools require Unix nonblocking pipes",
        ));
    }
    let assignments = tasks(&input, true)?;
    let name = required_text(&input, "agent", 128)?.to_string();
    if name == "seance" {
        return Err(invalid(
            "historical consultation cannot become a reusable pool",
        ));
    }
    // Pool tasks are strings governed by one frozen workspace. The ordinary
    // batch validator instead expects workspace controls on each task object.
    let mut workspace_controls = serde_json::Map::new();
    for field in ["cwd", "isolation"] {
        if let Some(value) = input.get(field) {
            workspace_controls.insert(field.to_string(), value.clone());
        }
    }
    workspace::validate_request(&Value::Object(workspace_controls))?;
    if input
        .get("isolation")
        .is_some_and(|value| value.as_str() != Some("none"))
    {
        return Err(invalid("pools currently require isolation none"));
    }
    let concurrency =
        usize::try_from(optional_integer(&input, "concurrency", 1, MAX_CHILDREN)?.unwrap_or(4))
            .map_err(|_| invalid("concurrency must be from 1 to 8"))?;
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
    let agent = tool
        .discover(scope)?
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
    let cwd = std::fs::canonicalize(selected_cwd)
        .map_err(|error| Error::tool("subagent", format!("PI_SUBAGENT_POOL_CWD: {error}")))?;
    if !cwd.is_dir() {
        return Err(invalid("cwd must name an existing directory"));
    }
    recheck_owner(tool, &owner, &owner_id, deadline).await?;
    let workers: Vec<_> = (0..concurrency)
        .map(|_| worker(&owner_id, &name, deadline))
        .collect();
    let id = format!("pool-{}", uuid::Uuid::new_v4().simple());
    let pool = Arc::new(Mutex::new(PoolState {
        owner: owner_id.clone(),
        id: id.clone(),
        job_id: String::new(),
        agent: name,
        deadline,
        acknowledged: false,
        sealed: false,
        closing: false,
        closed: false,
        failed: false,
        workers,
        items: Vec::new(),
        privacy: Vec::new(),
    }));
    register(Arc::clone(&pool))?;
    let children = (0..concurrency)
        .map(|_| {
            (
                ChildRunner::new(
                    cwd.clone(),
                    tool.global_dir.clone(),
                    tool.child_binary.clone(),
                    tool.role_model_spec.clone(),
                    crate::agent_hub::ChildKind::Subagent,
                    deadline,
                )
                .with_parent_model(parent_model.clone()),
                agent.clone(),
            )
        })
        .collect();
    let artifacts = tool.global_dir.join("tool-output-artifacts");
    // jobs list remains available after a guest reset or lost initial ACK.
    // Its owner-scoped label carries the recoverable native pool identity.
    let label = format!("reusable subagent pool {id}");
    let job = crate::jobs::spawn_owned_task(
        admission,
        &label,
        owner.clone(),
        deadline.remaining(),
        Some(&artifacts),
        privacy.clone(),
        run(Arc::clone(&pool), children, privacy.clone()),
    );
    let job = match job {
        Ok(job) => job,
        Err(error) => {
            registry()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|candidate| !Arc::ptr_eq(candidate, &pool));
            return Err(error);
        }
    };
    {
        let mut state = pool.lock().map_err(|_| stopped())?;
        state.job_id.clone_from(&job.id);
        for worker in &state.workers {
            worker
                .lock()
                .map_err(|_| stopped())?
                .job_id
                .clone_from(&job.id);
        }
    }
    // The controller cannot even start its RPC children until this complete
    // identity acknowledgment is screened and initial admission is committed.
    match admit(
        tool,
        &pool,
        &assignments,
        &privacy,
        &owner,
        &owner_id,
        deadline,
        true,
    )
    .await
    {
        Ok(output) => Ok(output),
        Err(error) => {
            close(&pool).await?;
            Err(error)
        }
    }
}

async fn recheck_owner(
    tool: &SubagentTool,
    owner: &AgentCx,
    id: &str,
    deadline: Deadline,
) -> Result<()> {
    check_owner(owner)?;
    if background::resolve_session(&tool.job_session_scope, owner, deadline).await? != id {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_SESSION_CHANGED: pool owner changed during the operation",
        ));
    }
    check_owner(owner)
}

struct Batch {
    base: usize,
    placements: Vec<(usize, u64)>,
    acknowledgment: ToolOutput,
}

fn prepare_batch(
    pool: &Pool,
    count: usize,
    privacy: &AuxiliaryPrivacySnapshot,
    initial: bool,
) -> Result<Batch> {
    let (base, placements, value, policies) = {
        let state = pool.lock().map_err(|_| stopped())?;
        ensure_admission(&state, count, initial)?;
        let mut loads = state
            .workers
            .iter()
            .enumerate()
            .map(|(index, worker)| {
                let worker = worker.lock().map_err(|_| stopped())?;
                let active = worker
                    .turns
                    .values()
                    .filter(|turn| pending(turn.status))
                    .count();
                Ok((
                    index,
                    active,
                    worker.latest,
                    !worker.closed && !worker.closing,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut placements = Vec::with_capacity(count);
        for _ in 0..count {
            let candidate = loads
                .iter_mut()
                .filter(|load| load.3 && load.2 < MAX_TURNS)
                .min_by_key(|load| (load.1, load.2, load.0))
                .ok_or_else(stopped)?;
            candidate.1 += 1;
            candidate.2 += 1;
            placements.push((candidate.0, candidate.2));
        }
        let mut value = manifest(&state, None, false)?;
        let ids: Vec<_> = (state.items.len() + 1..=state.items.len() + count).collect();
        let items = value
            .get_mut("items")
            .and_then(Value::as_array_mut)
            .ok_or_else(stopped)?;
        for id in &ids {
            items.push(json!({"id":id, "status":"queued", "summary":"", "resultRef":{"poolId":state.id,"itemId":id}}));
        }
        let fields = value.as_object_mut().ok_or_else(stopped)?;
        let queued = fields
            .get("queuedItems")
            .and_then(Value::as_u64)
            .ok_or_else(stopped)?;
        fields.insert("admittedItemIds".to_string(), json!(ids));
        fields.insert(
            "admittedItems".to_string(),
            json!(state.items.len() + count),
        );
        fields.insert(
            "queuedItems".to_string(),
            json!(queued + u64::try_from(count).map_err(|_| stopped())?),
        );
        (state.items.len(), placements, value, state.privacy.clone())
    };
    Ok(Batch {
        base,
        placements,
        acknowledgment: projected_snapshot_with(value, &policies, privacy)?,
    })
}

fn ensure_admission(state: &PoolState, count: usize, initial: bool) -> Result<()> {
    if state.closed
        || state.closing
        || state.sealed
        || state.deadline.check().is_err()
        || state.acknowledged == initial
    {
        return Err(stopped());
    }
    if state.items.len().saturating_add(count) > MAX_ITEMS {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_POOL_ITEM_LIMIT: pool reached its 32-item lifetime limit",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn admit(
    tool: &SubagentTool,
    pool: &Pool,
    assignments: &[String],
    privacy: &AuxiliaryPrivacySnapshot,
    owner: &AgentCx,
    owner_id: &str,
    budget: Deadline,
    initial: bool,
) -> Result<ToolOutput> {
    for _ in 0..MAX_ITEMS {
        let batch = prepare_batch(pool, assignments.len(), privacy, initial)?;
        recheck_owner(tool, owner, owner_id, budget).await?;
        let job_id = pool.lock().map_err(|_| stopped())?.job_id.clone();
        let accepted = crate::jobs::with_owned_task_progress(owner_id, &job_id, || {
            let mut state = pool.lock().map_err(|_| stopped())?;
            ensure_admission(&state, assignments.len(), initial)?;
            if state.items.len() != batch.base {
                return Ok((false, None));
            }
            // Linearize the whole batch under jobs -> pool -> all member
            // locks, in index order. No item is queued until every placement
            // has been checked, so a failure cannot admit a partial batch.
            let members = state.workers.clone();
            let mut workers = members
                .iter()
                .map(|worker| worker.lock().map_err(|_| stopped()))
                .collect::<Result<Vec<_>>>()?;
            let mut latest: Vec<_> = workers.iter().map(|worker| worker.latest).collect();
            for &(index, turn) in &batch.placements {
                let worker = workers.get(index).ok_or_else(stopped)?;
                let latest = latest.get_mut(index).ok_or_else(stopped)?;
                if worker.closed || worker.closing || *latest + 1 != turn {
                    return Ok((false, None));
                }
                *latest = turn;
            }
            for ((index, id), task) in batch.placements.iter().zip(assignments) {
                // Every placement was checked above; the locked member
                // vector is unchanged throughout the winning commit.
                let worker = workers.get_mut(*index).ok_or_else(stopped)?;
                worker.latest = *id;
                worker.turns.insert(
                    *id,
                    Turn {
                        status: "queued",
                        result: None,
                        privacy: Some(privacy.clone()),
                    },
                );
                worker.queue.push_back(PendingTurn {
                    id: *id,
                    task: task.clone(),
                }); // ubs:ignore[rust.collections.clone-in-loop] -- Only the winning atomic admission copies each distinct accepted task into its native owned queue.
                state.items.push(Item {
                    worker: *index,
                    turn: *id,
                });
            }
            state.privacy.push(privacy.clone());
            if initial {
                for worker in &mut workers {
                    worker.acknowledged = true;
                }
                state.acknowledged = true;
            }
            Ok((true, None))
        })?;
        if accepted {
            return Ok(batch.acknowledgment);
        }
    }
    Err(invalid(
        "concurrent admissions exhausted the pool's item budget",
    ))
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
    if operation == "pool_start" {
        return start(tool, input, privacy).await;
    }
    let allowed: &[&str] = match operation {
        "pool_push" => &["operation", "poolId", "tasks"],
        "pool_finish" | "pool_status" | "pool_close" => &["operation", "poolId"],
        "pool_result" => &["operation", "poolId", "itemId"],
        "pool_wait" => &["operation", "poolId", "itemId", "timeoutMs"],
        _ => return Err(invalid("unknown pool operation")),
    };
    validate_fields(&input, allowed)?;
    let id = required_text(&input, "poolId", 128)?;
    let selected = optional_integer(&input, "itemId", 1, MAX_TURNS)?;
    if operation == "pool_result" && selected.is_none() {
        return Err(invalid("pool_result requires itemId"));
    }
    let assignments = if operation == "pool_push" {
        Some(tasks(&input, false)?)
    } else {
        None
    };
    let timeout_ms = optional_integer(&input, "timeoutMs", 0, 600_000)?.unwrap_or(30_000);
    let owner = AgentCx::for_current_or_request();
    check_owner(&owner)?;
    let budget = Deadline::for_request(Some(Duration::from_millis(timeout_ms + 30_000)), None)?;
    let owner_id = background::resolve_session(&tool.job_session_scope, &owner, budget).await?;
    let pool = lookup(&owner_id, id)?;
    if let Some(assignments) = assignments {
        return admit(
            tool,
            &pool,
            &assignments,
            &privacy,
            &owner,
            &owner_id,
            budget,
            false,
        )
        .await;
    }
    if operation == "pool_finish" {
        return seal(tool, &pool, &privacy, &owner, &owner_id, budget).await;
    }
    if operation == "pool_close" {
        recheck_owner(tool, &owner, &owner_id, budget).await?;
        close(&pool).await?;
    }
    let started = Instant::now();
    loop {
        recheck_owner(tool, &owner, &owner_id, budget).await?;
        let output = snapshot(&pool, selected, &privacy)?;
        let details = output.details.as_ref().ok_or_else(stopped)?;
        let waiting = if selected.is_some() {
            details
                .pointer("/item/status")
                .and_then(Value::as_str)
                .is_some_and(pending)
        } else {
            details
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| matches!(status, "open" | "draining"))
        };
        if operation != "pool_wait"
            || !waiting
            || started.elapsed() >= Duration::from_millis(timeout_ms)
        {
            return Ok(output);
        }
        owner
            .time()
            .sleep(
                POLL_INTERVAL
                    .min(Duration::from_millis(timeout_ms).saturating_sub(started.elapsed())),
            )
            .await;
    }
}

async fn seal(
    tool: &SubagentTool,
    pool: &Pool,
    privacy: &AuxiliaryPrivacySnapshot,
    owner: &AgentCx,
    owner_id: &str,
    budget: Deadline,
) -> Result<ToolOutput> {
    // At most 32 nonempty pushes can win between preparations. The extra
    // iteration observes the resulting sealed or full pool without replay.
    for _ in 0..=MAX_ITEMS {
        let prepared = {
            let state = pool.lock().map_err(|_| stopped())?;
            if state.sealed || state.closed {
                None
            } else {
                let mut value = manifest(&state, None, false)?;
                let fields = value.as_object_mut().ok_or_else(stopped)?;
                fields.insert("sealed".to_string(), json!(true));
                fields.insert("status".to_string(), json!("draining"));
                Some((
                    state.job_id.clone(),
                    state.items.len(),
                    value,
                    state.privacy.clone(),
                ))
            }
        };
        let Some((job_id, revision, value, policies)) = prepared else {
            recheck_owner(tool, owner, owner_id, budget).await?;
            return snapshot(pool, None, privacy);
        };
        let acknowledgment = projected_snapshot_with(value, &policies, privacy)?;
        recheck_owner(tool, owner, owner_id, budget).await?;
        let accepted = crate::jobs::with_owned_task_progress(owner_id, &job_id, || {
            let mut state = pool.lock().map_err(|_| stopped())?;
            if state.sealed || state.closed {
                return Ok((false, None));
            }
            if state.closing || state.deadline.check().is_err() {
                return Err(stopped());
            }
            if state.items.len() != revision {
                // A newly accepted batch also adds a privacy policy. Prepare
                // both membership and privacy again before sealing it.
                return Ok((false, None));
            }
            state.sealed = true;
            Ok((true, None))
        });
        match accepted {
            Ok(true) => return Ok(acknowledgment),
            Ok(false) => {}
            Err(error) => {
                let settled = {
                    let state = pool.lock().map_err(|_| stopped())?;
                    state.sealed || state.closed
                };
                if !settled {
                    return Err(error);
                }
                recheck_owner(tool, owner, owner_id, budget).await?;
                return snapshot(pool, None, privacy);
            }
        }
    }
    Err(invalid(
        "concurrent admissions prevented sealing within the pool's item budget",
    ))
}

async fn close(pool: &Pool) -> Result<()> {
    let (owner, job_id) = {
        let mut state = pool.lock().map_err(|_| stopped())?;
        if state.closed {
            return Ok(());
        }
        state.closing = true;
        state.sealed = true;
        (state.owner.clone(), state.job_id.clone())
    };
    // No private lock may be held while entering jobs. Its cancellation await
    // returns after PoolRun drops and reaps every child, including idle ones.
    if let Err(error) = crate::jobs::cancel_async(&owner, &job_id).await {
        if pool.lock().map_err(|_| stopped())?.closed {
            return Ok(());
        }
        let settled = crate::jobs::wait_async(&owner, &job_id, Duration::from_secs(10)).await?;
        if settled.status == "running" {
            return Err(error);
        }
    }
    Ok(())
}

fn pending(status: &str) -> bool {
    matches!(status, "queued" | "running")
}

fn status(state: &PoolState, terminal: bool) -> &'static str {
    if state.closed || terminal {
        if state.closing {
            "cancelled"
        } else if state.failed {
            "failed"
        } else {
            "completed"
        }
    } else if state.sealed {
        "draining"
    } else {
        "open"
    }
}

fn summary(result: Option<&ToolOutput>) -> String {
    let Some(result) = result else {
        return String::new();
    };
    let text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    super::super::truncated_field(&text, SUMMARY_BYTES)
}

fn manifest(state: &PoolState, selected: Option<u64>, terminal: bool) -> Result<Value> {
    // One member must not appear to run two different turns in the same
    // snapshot if it completes a turn between reading adjacent item rows.
    // Keep the same index-ordered member locking used by batch admission.
    let workers = state
        .workers
        .iter()
        .map(|worker| worker.lock().map_err(|_| stopped()))
        .collect::<Result<Vec<_>>>()?;
    let mut items = Vec::with_capacity(state.items.len());
    let mut selected_item = None;
    let mut queued = 0;
    let mut running = 0;
    let mut completed = 0;
    let mut failed = 0;
    for (index, item) in state.items.iter().enumerate() {
        let worker = workers.get(item.worker).ok_or_else(stopped)?;
        let turn = worker.turns.get(&item.turn).ok_or_else(stopped)?;
        let id = u64::try_from(index + 1).map_err(|_| stopped())?;
        match turn.status {
            "queued" => queued += 1,
            "running" => running += 1,
            "completed" => completed += 1,
            _ => failed += 1,
        }
        items.push(json!({"id":id, "status":turn.status, "summary":summary(turn.result.as_ref()), "resultRef":{"poolId":state.id,"itemId":id}}));
        if selected == Some(id) {
            selected_item = Some(json!({"id":id, "status":turn.status, "result":turn.result}));
        }
    }
    let mut value = json!({
        "schema":SCHEMA, "poolId":state.id, "jobId":state.job_id, "agent":state.agent,
        "status":status(state, terminal), "sealed":state.sealed, "concurrency":state.workers.len(),
        "admittedItems":state.items.len(), "queuedItems":queued, "runningItems":running,
        "completedItems":completed, "failedItems":failed, "items":items,
        "remainingMs":u64::try_from(state.deadline.remaining().as_millis()).unwrap_or(u64::MAX),
    });
    if let Some(id) = selected {
        let item = selected_item.ok_or_else(|| invalid("itemId was not admitted by this pool"))?;
        let fields = value.as_object_mut().ok_or_else(stopped)?;
        fields.insert("itemId".to_string(), json!(id));
        fields.insert("item".to_string(), item);
    }
    Ok(value)
}

fn snapshot(
    pool: &Pool,
    selected: Option<u64>,
    privacy: &AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    let (value, policies) = {
        let state = pool.lock().map_err(|_| stopped())?;
        (manifest(&state, selected, false)?, state.privacy.clone())
    };
    projected_snapshot_with(value, &policies, privacy)
}

fn projected_snapshot_with(
    value: Value,
    policies: &[AuxiliaryPrivacySnapshot],
    privacy: &AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    let mut details = value.clone();
    for policy in policies.iter().chain(std::iter::once(privacy)) {
        details = policy.project_json(&details)?;
    }
    let unchanged = [
        "schema",
        "poolId",
        "jobId",
        "agent",
        "status",
        "sealed",
        "concurrency",
        "admittedItems",
        "queuedItems",
        "runningItems",
        "completedItems",
        "failedItems",
        "itemId",
        "admittedItemIds",
    ]
    .iter()
    .all(|key| value.get(key) == details.get(key));
    let items_preserved = value
        .get("items")
        .and_then(Value::as_array)
        .zip(details.get("items").and_then(Value::as_array))
        .is_some_and(|(before, after)| {
            before.len() == after.len()
                && before.iter().zip(after).all(|(before, after)| {
                    ["id", "status", "resultRef"]
                        .iter()
                        .all(|key| before.get(key) == after.get(key))
                        && after.get("summary").is_some_and(Value::is_string)
                })
        });
    if !unchanged
        || !items_preserved
        || value.pointer("/item/id") != details.pointer("/item/id")
        || value.pointer("/item/status") != details.pointer("/item/status")
    {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_POOL_PRIVACY: privacy policy withheld pool protocol identity",
        ));
    }
    let state = details
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unavailable");
    let mut text = format!(
        "Reusable pool is {state}. pool_finish seals and drains admitted work; pool_close cancels it. Use pool_result with poolId/itemId for a full native result."
    );
    if let Some(blocks) = details
        .pointer("/item/result/content")
        .and_then(Value::as_array)
    {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(answer) = block.get("text").and_then(Value::as_str) {
                    text.push('\n');
                    text.push_str(answer);
                }
            }
        }
    } else if let Some(items) = details.get("items").and_then(Value::as_array) {
        for item in items {
            let id = item.get("id").and_then(Value::as_u64).unwrap_or_default();
            let state = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unavailable");
            let summary = item
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default();
            text.push_str(&format!("\n[{id} {state}] {summary}"));
        }
    }
    let output = ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(
            super::super::truncated_field(&text, 16 * 1024),
        ))],
        details: Some(details),
        // Observations must remain usable when an item failed. The carrier's
        // final aggregate separately marks failed completion for jobs.
        is_error: false,
    };
    if serde_json::to_vec(&output).map_err(|_| stopped())?.len() > MAX_SNAPSHOT_BYTES {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_POOL_RESULT_LIMIT: projected pool snapshot exceeded its bounded result budget",
        ));
    }
    Ok(output)
}

/// Own the actor futures before returning the carrier future, including when
/// jobs rejects admission without ever polling it. Drop always retires actors
/// before publishing pool terminal state.
struct PoolRun {
    pool: Pool,
    actors: Option<FuturesUnordered<ActorFuture>>,
    settled: bool,
}

fn run(
    pool: Pool,
    children: Vec<(ChildRunner, AgentDefinition)>,
    privacy: AuxiliaryPrivacySnapshot,
) -> impl Future<Output = Result<ToolOutput>> + Send {
    let mut controller = PoolRun {
        pool,
        actors: Some(FuturesUnordered::new()),
        settled: false,
    };
    async move { controller.execute(children, &privacy).await }
}

impl PoolRun {
    async fn execute(
        &mut self,
        children: Vec<(ChildRunner, AgentDefinition)>,
        privacy: &AuxiliaryPrivacySnapshot,
    ) -> Result<ToolOutput> {
        let owner = AgentCx::for_current_or_request();
        loop {
            check_owner(&owner)?;
            let ready = {
                let state = self.pool.lock().map_err(|_| stopped())?;
                if state.closing || state.closed || state.deadline.check().is_err() {
                    return Err(stopped());
                }
                state.acknowledged
            };
            if ready {
                break;
            }
            owner.time().sleep(POLL_INTERVAL).await;
        }
        let workers = self.pool.lock().map_err(|_| stopped())?.workers.clone();
        for ((runner, agent), worker) in children.into_iter().zip(workers) {
            let actor = execution::worker::run(runner, agent, worker);
            self.actors
                .as_mut()
                .ok_or_else(stopped)?
                .push(Box::pin(async move {
                    // Every actor records its terminal state only after process
                    // reap. Raw child diagnostics never enter an unscreened notice.
                    let _ = actor.await;
                }));
        }
        loop {
            check_owner(&owner)?;
            if self.monitor()? {
                drop(self.actors.take());
                return self.complete(privacy);
            }
            let actors = self.actors.as_mut().ok_or_else(stopped)?;
            let next = Box::pin(actors.next());
            let time = owner.time();
            let sleep = Box::pin(time.sleep(POLL_INTERVAL));
            let _ = futures::future::select(next, sleep).await;
        }
    }

    fn monitor(&self) -> Result<bool> {
        let mut state = self.pool.lock().map_err(|_| stopped())?;
        if state.closing || state.closed || state.deadline.check().is_err() {
            return Err(stopped());
        }
        let mut all_closed = true;
        let mut failed = false;
        for worker in &state.workers {
            let mut worker = worker.lock().map_err(|_| stopped())?;
            let pending = worker.turns.values().any(|turn| pending(turn.status));
            failed |= worker.failed
                || worker
                    .turns
                    .values()
                    .any(|turn| matches!(turn.status, "failed" | "cancelled"));
            if state.sealed && !pending {
                worker.closing = true;
            }
            all_closed &= worker.closed;
        }
        state.failed |= failed;
        if all_closed {
            state.sealed = true;
        }
        Ok(all_closed)
    }

    fn complete(&mut self, privacy: &AuxiliaryPrivacySnapshot) -> Result<ToolOutput> {
        let (owner, job_id, value, policies) = {
            let state = self.pool.lock().map_err(|_| stopped())?;
            (
                state.owner.clone(),
                state.job_id.clone(),
                manifest(&state, None, true)?,
                state.privacy.clone(),
            )
        };
        let mut output = projected_snapshot_with(value, &policies, privacy)?;
        output.is_error = output
            .details
            .as_ref()
            .and_then(|details| details.get("status"))
            .and_then(Value::as_str)
            == Some("failed");
        crate::jobs::with_owned_task_progress(&owner, &job_id, || {
            let mut state = self.pool.lock().map_err(|_| stopped())?;
            if state.closing || state.closed || state.deadline.check().is_err() {
                return Err(stopped());
            }
            state.closed = true;
            Ok(((), None))
        })?;
        self.settled = true;
        Ok(output)
    }
}

impl Drop for PoolRun {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let workers = {
            let mut state = self
                .pool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closing = true;
            state.sealed = true;
            for worker in &state.workers {
                worker
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .closing = true;
            }
            state.workers.clone()
        };
        drop(self.actors.take());
        for worker in workers {
            super::finish(&worker, None);
        }
        self.pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::text_completion::AuxiliaryPrivacy;
    use crate::tools::Tool as _;
    use std::os::unix::fs::PermissionsExt as _;

    struct Fixture {
        root: tempfile::TempDir,
        tool: SubagentTool,
        owner: String,
    }

    impl Fixture {
        fn new(script: &str) -> Self {
            let root = tempfile::tempdir().expect("pool fixture root"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Real lifecycle tests require isolated process and artifact paths.
            let global = root.path().join("global");
            std::fs::create_dir_all(global.join("agents")).expect("agent directory"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The fixture must not discover ambient agent definitions.
            std::fs::write(global.join("agents/worker.md"), "---\nname: worker\ndescription: pool fixture\ntools: []\n---\nRemember previous assignments.\n").expect("agent definition"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- This authored agent is a prerequisite of actual native tool admission.
            let binary = root.path().join("child.sh");
            std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).expect("RPC child"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Use a controlled real process rather than mock future completion.
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))
                .expect("executable child"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The test transport must be executable.
            let owner = format!("pool-fixture-{}", uuid::Uuid::new_v4());
            let mut tool = SubagentTool::with_paths(root.path().to_path_buf(), global, binary)
                .with_timeout(Duration::from_secs(15));
            tool.bind_job_session_scope(crate::jobs::JobSessionScope::fixed(&owner));
            Self { root, tool, owner }
        }

        async fn call(&self, input: Value) -> Value {
            self.tool
                .execute("pool-fixture", input, None)
                .await
                .expect("native pool operation") // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Each observed handle must come from the public tool path.
                .details
                .expect("pool details") // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Structured native identity and result metadata are the tested contract.
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
                "controlled child boundary was not reached"
            ); // ubs:ignore[rust.panic.assert-macros] -- A gate proves the child is executing before cancellation or dynamic admission.
        }

        fn release(&self) {
            std::fs::write(self.root.path().join("release"), b"continue")
                .expect("release RPC child"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- This explicit gate releases real accepted work.
        }

        fn assert_reaped(&self) {
            let launches =
                std::fs::read_to_string(self.root.path().join("launches")).expect("child PIDs"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Independently recorded PIDs prove cleanup of each actual process.
            for pid in launches.lines() {
                let pid = pid
                    .parse::<i32>()
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
                    .expect("recorded PID"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- A malformed fixture PID invalidates lifecycle evidence.
                assert!(
                    rustix::process::test_kill_process(pid).is_err(),
                    "pool returned before reaping a member"
                ); // ubs:ignore[rust.panic.assert-macros] -- Terminal pool publication must follow real child reap.
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(runtime) = asupersync::runtime::RuntimeBuilder::current_thread().build() {
                let _ = runtime.block_on(crate::jobs::kill_session(&self.owner));
            }
        }
    }

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native pool runtime") // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The actual native jobs and RPC actors run on their production executor.
    }

    fn field(value: &Value, key: &str) -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .expect("native pool identity")
            .to_string() // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Tests must not synthesize missing returned identities.
    }

    fn native_result(value: &Value) -> &Value {
        value
            .pointer("/item/result/details/results/0")
            .expect("completed native item") // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The result must be produced by the real child, not by its admission acknowledgment.
    }

    fn private_projection(secret: &str) -> AuxiliaryPrivacySnapshot {
        let mut vault = crate::secrets::SecretVault::default();
        let pattern = regex::Regex::new(secret).expect("controlled secret pattern"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- This literal pattern records the later-cell secret discovery.
        crate::secrets::obfuscate(secret, &mut vault, &[pattern]);
        AuxiliaryPrivacySnapshot::with_vault(AuxiliaryPrivacy::from_settings(None), &vault)
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
            printf '%s\n' "$line" >> prompts
            printf '{"type":"response","id":"%s","command":"prompt","success":true}\n' "$id"
            printf '%s\n' '{"type":"agent_start"}'
            case "$line" in
                *'Task: remember silver'*) memory=silver; printf ready > silver-running ;;
                *'Task: remember gold'*) memory=gold; printf ready > gold-running ;;
            esac
            case "$line" in
                *'Task: remember '*|*'Task: hold'*)
                    printf ready > "running-$$"
                    while [ ! -f release ]; do sleep 0.02; done
                    ;;
            esac
            printf '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"%s"}]}]}\n' "$memory"
            busy=1
            ;;
    esac
done
"#;

    #[test]
    fn pool_drains_dynamic_batches_in_order_with_one_carrier_and_one_notice() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        runtime().block_on(async {
            let started = fixture.call(json!({"operation":"pool_start","agent":"worker","concurrency":2,"tasks":["remember silver","remember gold"],"cwd":".","isolation":"none"})).await;
            let pool = field(&started, "poolId");
            let job = field(&started, "jobId");
            fixture.await_file("silver-running").await;
            fixture.await_file("gold-running").await;
            assert_eq!(crate::jobs::list(&fixture.owner).expect("owned jobs").len(), 1); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- Multiple real members share exactly one native lifecycle carrier.
            let pushed = fixture.call(json!({"operation":"pool_push","poolId":pool,"tasks":["recall","recall"]})).await;
            assert_eq!(pushed.get("admittedItemIds"), Some(&json!([3,4]))); // ubs:ignore[rust.panic.assert-macros] -- Global pool IDs remain stable and preserve atomic batch order.
            let sealed = fixture.call(json!({"operation":"pool_finish","poolId":pool})).await;
            assert_eq!(sealed.get("status").and_then(Value::as_str), Some("draining")); // ubs:ignore[rust.panic.assert-macros] -- Finish seals admission without cancelling gated work.
            assert!(fixture.tool.execute("late-push", json!({"operation":"pool_push","poolId":pool,"tasks":["must not run"]}), None).await.is_err()); // ubs:ignore[rust.panic.assert-macros] -- Sealed pools cannot accept later assignments.
            assert!(crate::jobs::take_completion_notices(&fixture.owner).is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Neither member admission nor individual turns deliver extra pool notices.
            fixture.release();
            let drained = fixture.call(json!({"operation":"pool_wait","poolId":pool,"timeoutMs":5000})).await;
            assert_eq!(drained.get("status").and_then(Value::as_str), Some("completed")); // ubs:ignore[rust.panic.assert-macros] -- All admitted work drains to a successful terminal manifest.
            fixture.assert_reaped();
            let mut results = Vec::new();
            for item in 1..=4 {
                results.push(fixture.call(json!({"operation":"pool_result","poolId":pool,"itemId":item})).await);
            }
            for (result, expected) in results.iter().zip(["silver", "gold", "silver", "gold"]) {
                assert_eq!(native_result(result).get("output").and_then(Value::as_str), Some(expected)); // ubs:ignore[rust.panic.assert-macros] -- Follow-up assignments require memory from the same warm child.
            }
            let [first, second, third, fourth]: [Value; 4] = results.try_into().expect("four admitted results"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect,rust.casts.try-into-unwrap] -- The four explicit result reads above must all remain present for native reuse assertions.
            assert_eq!(native_result(&first).get("pid"), native_result(&third).get("pid")); // ubs:ignore[rust.panic.assert-macros] -- A queued item must reuse its original member process.
            assert_eq!(native_result(&second).get("pid"), native_result(&fourth).get("pid")); // ubs:ignore[rust.panic.assert-macros] -- The second member also retains its own conversation.
            let settled = crate::jobs::wait_async(&fixture.owner, &job, Duration::from_secs(5)).await.expect("aggregate job"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The aggregate is independently readable through the ordinary jobs API.
            assert_eq!(settled.status, "exited"); // ubs:ignore[rust.panic.assert-macros] -- Drained pool completion must settle its sole carrier successfully.
            let aggregate = settled.result.expect("aggregate result").details.expect("aggregate manifest"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Completion must retain the ordered native manifest.
            assert_eq!(aggregate.get("items"), drained.get("items")); // ubs:ignore[rust.panic.assert-macros] -- Jobs and native pool reads agree on stable item ordering and outcomes.
            assert!(aggregate.pointer("/items/0/result").is_none()); // ubs:ignore[rust.panic.assert-macros] -- Aggregate completion references full results without exceeding the carrier result budget.
            assert_eq!(crate::jobs::take_completion_notices(&fixture.owner).len(), 1); // ubs:ignore[rust.panic.assert-macros] -- Exactly one owner completion is delivered for the entire pool.
        });
    }

    #[test]
    fn closing_a_full_eight_member_pool_cancels_queued_work_and_reaps_every_process() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        runtime().block_on(async {
            let started = fixture.call(json!({"operation":"pool_start","agent":"worker","concurrency":8,"tasks":vec!["hold";32]})).await;
            let pool = field(&started, "poolId");
            let since = Instant::now();
            loop {
                let status = fixture.call(json!({"operation":"pool_status","poolId":pool})).await;
                if status.get("runningItems").and_then(Value::as_u64) == Some(8) { break; }
                assert!(since.elapsed() < Duration::from_secs(5), "eight members did not start"); // ubs:ignore[rust.panic.assert-macros] -- The test must exercise all eight native children, not just queued metadata.
                AgentCx::for_request().time().sleep(POLL_INTERVAL).await;
            }
            assert_eq!(crate::jobs::list(&fixture.owner).expect("pool job").len(), 1); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- Eight members must not consume eight carriers plus a tracking job.
            assert!(lookup("another-session", &pool).is_err()); // ubs:ignore[rust.panic.assert-macros] -- Pool identity is scoped to the originating session.
            let closed = fixture.call(json!({"operation":"pool_close","poolId":pool})).await;
            fixture.assert_reaped();
            assert_eq!(closed.get("status").and_then(Value::as_str), Some("cancelled")); // ubs:ignore[rust.panic.assert-macros] -- Close reports cancellation rather than successful drain.
            assert_eq!(closed.get("failedItems").and_then(Value::as_u64), Some(32)); // ubs:ignore[rust.panic.assert-macros] -- Active and queued items all settle before close returns.
            let selected = fixture.call(json!({"operation":"pool_result","poolId":pool,"itemId":32})).await;
            assert!(selected.pointer("/item/result").is_some_and(Value::is_object)); // ubs:ignore[rust.panic.assert-macros] -- A cancelled queued item retains an inspectable terminal error.
            let again = fixture.call(json!({"operation":"pool_close","poolId":pool})).await;
            assert_eq!(again.get("status"), closed.get("status")); // ubs:ignore[rust.panic.assert-macros] -- Closing an already reaped pool is idempotent.
        });
    }

    #[test]
    fn invalid_and_over_capacity_batches_admit_no_prefix() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        runtime().block_on(async {
            let started = fixture.call(json!({"operation":"pool_start","agent":"worker","concurrency":1,"tasks":["hold"]})).await;
            let pool = field(&started, "poolId");
            for assignments in [json!(["otherwise-valid", ""]), json!(vec!["overflow";32])] {
                assert!(fixture.tool.execute("rejected-batch", json!({"operation":"pool_push","poolId":pool,"tasks":assignments}), None).await.is_err()); // ubs:ignore[rust.panic.assert-macros] -- A bad tail or lifetime overflow must reject the complete batch.
                let status = fixture.call(json!({"operation":"pool_status","poolId":pool})).await;
                assert_eq!(status.get("admittedItems").and_then(Value::as_u64), Some(1)); // ubs:ignore[rust.panic.assert-macros] -- No valid prefix was enqueued by either rejected batch.
            }
            let accepted = fixture.call(json!({"operation":"pool_push","poolId":pool,"tasks":vec!["recall";31]})).await;
            assert_eq!(accepted.get("admittedItems").and_then(Value::as_u64), Some(32)); // ubs:ignore[rust.panic.assert-macros] -- Rejected batches do not consume future admission capacity.
            assert_eq!(accepted.get("admittedItemIds").and_then(Value::as_array).map(Vec::len), Some(31)); // ubs:ignore[rust.panic.assert-macros] -- Successful admission acknowledges every item in one response.
            fixture.call(json!({"operation":"pool_close","poolId":pool})).await;
        });
    }

    #[test]
    fn privacy_refusal_of_initial_identity_starts_no_child() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        runtime().block_on(async {
            let error = execute(
                &fixture.tool,
                json!({"operation":"pool_start","agent":"worker","tasks":["must not execute"]}),
                private_projection("worker"),
            )
            .await
            .expect_err("identity withheld"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- An undisclosable handle must fail before native process launch.
            assert!(error.to_string().contains("PI_SUBAGENT_POOL_PRIVACY")); // ubs:ignore[rust.panic.assert-macros] -- Do not turn a redacted agent identity into a usable-looking acknowledgment.
            assert!(!fixture.root.path().join("launches").exists()); // ubs:ignore[rust.panic.assert-macros] -- Privacy screening precedes all child process side effects.
            assert!(
                crate::jobs::list(&fixture.owner)
                    .expect("owned jobs")
                    .iter()
                    .all(|job| job.status != "running")
            ); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- Failed initial admission cannot leave an invisible live carrier.
        });
    }

    #[test]
    fn later_batch_privacy_protects_earlier_results_and_the_single_aggregate() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        fixture.release();
        runtime().block_on(async {
            let started = fixture.call(json!({"operation":"pool_start","agent":"worker","concurrency":1,"tasks":["remember silver"]})).await;
            let pool = field(&started, "poolId");
            let job = field(&started, "jobId");
            let before = fixture.call(json!({"operation":"pool_wait","poolId":pool,"itemId":1,"timeoutMs":5000})).await;
            assert_eq!(native_result(&before).get("output").and_then(Value::as_str), Some("silver")); // ubs:ignore[rust.panic.assert-macros] -- The first result predates the later invocation's secret discovery.
            execute(&fixture.tool, json!({"operation":"pool_push","poolId":pool,"tasks":["recall"]}), private_projection("silver")).await.expect("private batch admitted"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- A later accepted batch supplies its own immutable privacy snapshot.
            fixture.call(json!({"operation":"pool_finish","poolId":pool})).await;
            let settled = crate::jobs::wait_async(&fixture.owner, &job, Duration::from_secs(5)).await.expect("screened aggregate"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Aggregate delivery must use all accepted privacy snapshots.
            let encoded = serde_json::to_string(&settled.result).expect("complete aggregate JSON"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Inspect every aggregate field for a bare secret.
            assert!(!encoded.contains("silver")); // ubs:ignore[rust.panic.assert-macros] -- Later discoveries protect earlier summaries before final delivery.
            assert!(encoded.contains("<pi-secret:redacted>")); // ubs:ignore[rust.panic.assert-macros] -- Redaction must retain useful successful item results.
            let reread = fixture.call(json!({"operation":"pool_result","poolId":pool,"itemId":1})).await;
            assert!(!serde_json::to_string(&reread).expect("full protected item").contains("silver")); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- A weaker later reader cannot recover an earlier now-known secret.
            let notice = serde_json::to_string(&crate::jobs::take_completion_notices(&fixture.owner)).expect("notice JSON"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Completion notices are an independent durable output surface.
            assert!(!notice.contains("silver")); // ubs:ignore[rust.panic.assert-macros] -- The one aggregate notice follows the same privacy floor.
        });
    }

    #[test]
    fn finish_reprojects_a_batch_that_wins_during_owner_resolution() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        fixture.release();
        runtime().block_on(async {
            let started = fixture.call(json!({"operation":"pool_start","agent":"worker","concurrency":1,"tasks":["remember silver"]})).await;
            let pool_id = field(&started, "poolId");
            fixture.call(json!({"operation":"pool_wait","poolId":pool_id,"itemId":1,"timeoutMs":5000})).await;
            let pool = lookup(&fixture.owner, &pool_id).expect("native pool"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Use the same admitted native state for the controlled seal race.
            let (release, pending) = futures::channel::oneshot::channel::<()>();
            let pending = Arc::new(Mutex::new(Some(pending)));
            let owner_id = fixture.owner.clone();
            fixture.tool.job_session_scope.bind(Arc::new(move || {
                let pause = pending.lock().expect("test gate").take(); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- Poisoning this controlled gate invalidates the deterministic race fixture and must fail the test before any awaited receiver is created.
                let id = owner_id.clone();
                Box::pin(async move {
                    if let Some(pause) = pause { let _ = pause.await; }
                    Some(id)
                })
            }));
            let owner = AgentCx::for_request();
            let budget = Deadline::for_request(Some(Duration::from_secs(10)), None).expect("seal budget"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The fixture preserves a real bounded operation deadline.
            let privacy = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None));
            let mut sealing = Box::pin(seal(&fixture.tool, &pool, &privacy, &owner, &fixture.owner, budget));
            assert!(futures::poll!(&mut sealing).is_pending()); // ubs:ignore[rust.panic.assert-macros] -- Seal has prepared its old manifest and is paused before its commit fence.
            execute(&fixture.tool, json!({"operation":"pool_push","poolId":pool_id,"tasks":["recall"]}), private_projection("silver")).await.expect("concurrent admission"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The new batch and privacy snapshot must win before sealing.
            release.send(()).expect("resume sealing"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Release the exact owner-resolution boundary after real admission.
            let sealed = sealing.await.expect("reprojected seal").details.expect("seal manifest"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The winning finish must acknowledge the actual accepted membership.
            assert_eq!(sealed.get("admittedItems").and_then(Value::as_u64), Some(2)); // ubs:ignore[rust.panic.assert-macros] -- An older one-item manifest must not seal a larger pool.
            assert!(!serde_json::to_string(&sealed).expect("seal JSON").contains("silver")); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- The acknowledgment must also include the concurrent batch's stricter privacy.
            fixture.call(json!({"operation":"pool_wait","poolId":pool_id,"timeoutMs":5000})).await;
            let repeated = fixture.call(json!({"operation":"pool_finish","poolId":pool_id})).await;
            assert_eq!(repeated.get("status").and_then(Value::as_str), Some("completed")); // ubs:ignore[rust.panic.assert-macros] -- Finish stays idempotent after the carrier has drained and reaped.
        });
    }

    #[test]
    fn failed_member_is_not_replaced_and_queued_items_remain_inspectable() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(
            r#"
printf '%s\n' "$$" >> launches
while IFS= read -r line; do
    id=${line#*\"id\":\"}
    id=${id%%\"*}
    case "$line" in
        *'"type":"get_state"'*)
            printf '{"type":"response","id":"%s","command":"get_state","success":true,"data":{"isStreaming":false,"isCompacting":false}}\n' "$id"
            ;;
        *'"type":"prompt"'*)
            printf performed >> effects
            printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"unacknowledged terminal"}]}]}'
            ;;
    esac
done
"#,
        );
        runtime().block_on(async {
            let started = fixture.call(json!({"operation":"pool_start","agent":"worker","concurrency":1,"tasks":["effect then invalid protocol","must not replay"]})).await;
            let pool = field(&started, "poolId");
            let job = field(&started, "jobId");
            let failed = fixture.call(json!({"operation":"pool_wait","poolId":pool,"timeoutMs":5000})).await;
            assert_eq!(failed.get("status").and_then(Value::as_str), Some("failed")); // ubs:ignore[rust.panic.assert-macros] -- Losing every member settles the pool without waiting for explicit finish.
            assert_eq!(failed.get("failedItems").and_then(Value::as_u64), Some(2)); // ubs:ignore[rust.panic.assert-macros] -- The uncertain active item and unexecuted queue both retain terminal failures.
            assert_eq!(std::fs::read_to_string(fixture.root.path().join("effects")).expect("effect evidence"), "performed"); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- Ambiguous side effects must never cause a retry or replacement child.
            fixture.assert_reaped();
            let result = fixture.tool.execute("inspect-failure", json!({"operation":"pool_result","poolId":pool,"itemId":2}), None).await.expect("failed item inspection"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Failure observation is an accepted operation, not a protocol error.
            assert!(!result.is_error); // ubs:ignore[rust.panic.assert-macros] -- Callers may inspect terminal failure through normal pool_result.
            let selected = result.details.expect("selected error details"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- A failed queued item must remain addressable.
            assert!(selected.pointer("/item/result").is_some_and(Value::is_object)); // ubs:ignore[rust.panic.assert-macros] -- Failed items have concrete retained ToolOutput values.
            let settled = crate::jobs::wait_async(&fixture.owner, &job, Duration::from_secs(5)).await.expect("failed aggregate job"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The carrier independently reports failed aggregate completion.
            assert_eq!(settled.status, "failed"); // ubs:ignore[rust.panic.assert-macros] -- A failed pool must not appear successful through jobs.
        });
    }

    #[test]
    fn an_empty_pool_has_a_real_finishable_job() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = Fixture::new(RPC_CHILD);
        runtime().block_on(async {
            let started = fixture
                .call(json!({"operation":"pool_start","agent":"worker","concurrency":1,"tasks":[]}))
                .await;
            let pool = field(&started, "poolId");
            let job = field(&started, "jobId");
            assert_eq!(started.get("admittedItemIds"), Some(&json!([]))); // ubs:ignore[rust.panic.assert-macros] -- Empty start creates identity without inventing an accepted assignment.
            let listed = crate::jobs::list(&fixture.owner).expect("recoverable pool carrier"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Native recovery must be available independently of a guest handle.
            assert!(listed.iter().any(|entry| entry.id == job
                && entry.command == format!("reusable subagent pool {pool}"))); // ubs:ignore[rust.panic.assert-macros] -- Owner-scoped jobs listing recovers both stable IDs after a lost initial acknowledgment.
            fixture
                .call(json!({"operation":"pool_finish","poolId":pool}))
                .await;
            let settled = crate::jobs::wait_async(&fixture.owner, &job, Duration::from_secs(5))
                .await
                .expect("empty aggregate"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- Empty finish still owns and settles a real native job.
            assert_eq!(settled.status, "exited"); // ubs:ignore[rust.panic.assert-macros] -- No-work drain is a successful aggregate completion.
            let manifest = settled
                .result
                .expect("empty result")
                .details
                .expect("empty manifest"); // ubs:ignore[rust.panic.expect,rust.ownership.unwrap-expect] -- The aggregate is structurally identical for zero admitted items.
            assert_eq!(manifest.get("items"), Some(&json!([]))); // ubs:ignore[rust.panic.assert-macros] -- An empty pool must not manufacture member results.
            if fixture.root.path().join("launches").exists() {
                fixture.assert_reaped();
            }
        });
    }

    #[test]
    fn task_batches_reject_non_strings_and_oversized_assignments() {
        for value in [
            Value::Null,
            json!({}),
            json!(["valid", 4]),
            json!(["a\0b"]),
            json!(["x".repeat(MAX_PROMPT_BYTES + 1)]),
            json!([]),
        ] {
            assert!(tasks(&json!({"tasks":value}), false).is_err()); // ubs:ignore[rust.panic.assert-macros] -- Malformed batched assignments cannot be coerced or partially accepted.
        }
        assert!(tasks(&json!({}), true).expect("empty start").is_empty()); // ubs:ignore[rust.panic.assert-macros,rust.panic.expect,rust.ownership.unwrap-expect] -- Empty start explicitly establishes a recoverable native pool identity.
    }
}
