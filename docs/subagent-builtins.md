# Built-in native subagents

The opt-in `subagent` tool now works without installing agent Markdown files.
Its five bundled profiles use the existing native child runner:

| Agent | Purpose | Tools |
| --- | --- | --- |
| `task` | Implementation and multi-step delegated work | read, bash, edit, write, grep, find, ls, hashline_edit |
| `scout` | Locate relevant code, call paths and tests | read, grep, find, ls |
| `reviewer` | Find correctness defects and regressions | read, grep, find, ls |
| `security-reviewer` | Defensive security review of trust boundaries | read, grep, find, ls |
| `sonic` | Narrow mechanical updates or data collection | read, bash, edit, write, grep, find, ls, hashline_edit |

```json
{"agent":"scout","task":"Map the provider failover flow and identify its tests"}
```

```json
{"chain":[{"agent":"scout","task":"Locate the parser and describe the required fix"},{"agent":"task","task":"Implement the fix using this evidence: {previous}","isolation":"worktree","isoApply":"keep"}]}
```

Definitions are resolved in this order: built-in, user, nearest project, with
later definitions replacing earlier definitions of the same name. `scope`
selects which authored overrides are read; built-ins remain available for
`both`, `user`, and `project`. A malformed selected override remains an error,
not an excuse to silently use a different profile. Discovery writes no files.

Profiles have no model pins. They use the host's existing task/smol-role
selection, then the captured active parent model, then ordinary child
configuration. In particular, the `sonic` name does not automatically select a
new model or change thinking effort. Override its Markdown definition to pin
those settings. A user/project model pin retains its existing precedence.

The names match OMP v18.10.4's catalog, but these are native Pi prompts and
policies, not full OMP task-runtime parity. Bundled profiles do not enable
recursive delegation or hub control. The inspection profiles have no shell,
write, eval, or process tools. They inspect source; they cannot run tests.
These tool selections are not an operating-system sandbox or a substitute for
workspace trust policy. Authored overrides may intentionally select other tools.

Parallel and chained calls, structured-output validation and its bounded retry,
request-wide deadlines, inherited cancellation, hub bookkeeping and worktree
acceptance continue through the existing runner. Enabling this catalog does
not enable the opt-in tool globally and does not create or publish Git commits.

## Single-task workspace selection

Single-task calls now accept the same `cwd`, `isolation` and `isoApply` options
as entries in `tasks` and `chain`:

```json
{"agent":"task","task":"Implement the parser fix and run the permitted checks","cwd":"packages/parser","isolation":"worktree","isoApply":"keep"}
```

`cwd` is absolute or relative to the host workspace. In worktree mode its
repository-relative location is retained inside the captured checkout; a
missing/non-Git workspace fails instead of running in another directory.
`keep` retains accepted edits for inspection, `apply` uses existing serialized
writeback, and `drop` reports the patch without applying it. Omitting all three
options retains the previous unisolated host-directory default.

An explicit `isoApply` requires `isolation: "worktree"`. Nulls, wrong types,
unknown policy values, empty paths and NUL-containing paths are rejected
before agent discovery or process launch, including SDK calls that bypass the
tool JSON Schema. For `tasks`/`chain`, workspace options must be placed on each
entry; top-level options are rejected rather than ignored or broadcast. These
checks do not add path confinement or change authored-agent discovery roots.

## Background delegation

Add `"async": true` to a single, `tasks`, `chain`, or `workflow` request to release the
calling turn while the same native child runner continues:

```json
{
  "async": true,
  "tasks": [
    {"agent": "scout", "task": "Map provider retry and cancellation behavior"},
    {"agent": "reviewer", "task": "Review the pending parser changes"}
  ],
  "concurrency": 2,
  "timeoutSeconds": 300
}
```

The acknowledgment has `details.schema: "pi.subagent.background.v1"`, a
`jobId`, and its initial `job` snapshot. Manage that ID through the ordinary
`jobs` tool:

```json
{"action":"wait","jobId":"job-…","timeoutMs":30000}
```

`list` discovers jobs in the current session; `cancel` stops unfinished work.
A settled snapshot has `schema: "pi.host_job.v1"` and `result`, containing the
native tool result (`content`, `details`, and `isError` when true). Its
`result.details.results` keeps each task's output, hub ID, typed data, schema
acceptance and worktree outcome. A wait that reaches its own timeout returns a
still-running snapshot; it does not restart or cancel the delegation. Completion
also enters the owning session's existing background follow-up queue.

Agent definitions, model selection and one request deadline are captured before
the acknowledgment. Definition edits or model changes do not retarget queued
work. All batch members must name valid agents and valid output schemas before
any job is admitted. Parallel execution, chain substitution, retries, isolation
and ordered writeback still use the existing runner. Worktree baselines are
captured when that runner begins the request. Session shutdown and explicit
cancellation retire the native future and its unfinished child process leases;
effects already accepted or performed are not rolled back.

Background jobs share the existing eight-job capacity with shell jobs. Their
complete final result is privacy-projected before artifacts, retention or
follow-up delivery. The result has a 1 MiB encoded limit: an oversized or
unprojectable result is replaced by a named failure, never a successful partial
result. These jobs do not publish live partial tool output; child progress and
transcripts remain available through the agent hub after children register.

The `async` field must be boolean. `seance` remains foreground and explicitly
refuses `async: true`.
No callback, borrowed Agent or eval interpreter is retained by the background
job. Native tests in `src/subagents/background.rs` cover early return, typed
results, child reaping, whole-batch refusal, frozen definitions/models and
session changes/cancellation during admission. Rust acceptance remains
`dsr quality --tool pi_agent_rust`.

### Background dependency workflows

Use one job for a complete dependency graph, rather than manually starting and
polling each child. The workflow's `call`, `needs`, typed `inputs`, conditions,
selected-branch joins and bounded `forEach` expansion keep their foreground
semantics:

```json
{
  "async": true,
  "workflow": [
    {"id": "scan", "call": {"agent": "scout", "task": "Map the parser entry points"}},
    {"id": "review", "needs": ["scan"], "call": {"agent": "reviewer", "task": "Review the parser for correctness"}},
    {"id": "independent", "call": {"agent": "scout", "task": "Locate the cancellation tests"}}
  ],
  "concurrency": 2,
  "timeoutSeconds": 300
}
```

`needs` specifies execution order; it does not automatically insert predecessor
text into another assignment. Use explicit `inputs` from schema-validated
predecessor data when a dependent needs those values. Workspace options belong
inside each node's `call`, not on the top-level graph.

The initial receipt has `details.mode: "workflow"`. A settled `jobs wait` result
retains `result.details.schema: "pi.subagent.workflow.v1"` and the authored-order
`nodes` array, including state, wave, blocked dependencies, native result,
schema verdict, hub ID and any fan-out item reports. The graph's result is not
flattened into the ordinary single/batch `results` array. Failed branches remain
errors; skipped descendants do not execute, and independent branches can finish.

The full graph is parsed and all agent definitions and required schemas are
validated before publication. Definition/model selection freezes once; one
absolute request deadline spans owner lookup, discovery, queueing, every wave
and all native retries. Every wave still uses the native runner's shared
worktree baseline and ordered acceptance barrier. Baselines are captured when
the wave begins, not at the initial acknowledgment.

Cancellation and session shutdown drop the same owned graph future and its
unfinished native child leases. There is no graph replay, restart persistence,
graph-wide rollback or automatic resume. Interrupted jobs use the jobs runtime's
explicit failure result; a partially completed graph report is not promised.
Accepted filesystem effects may remain. The existing privacy projection and
1 MiB complete-result limit apply to graph reports too. Native regression
coverage is in `src/subagents/workflow/background_tests.rs`; it is not a claim
that the DSR lane has run in a source-only editing environment.

## Reusable workers

Use `worker_start` when successive tasks should share one live child agent's
conversation. The child stays in RPC mode between turns, preserving its process,
provider setup and message history. Initial discovery captures the agent
definition, model, tool selection and working directory for the worker's lifetime.

```json
{"operation":"worker_start","agent":"scout","task":"Map the provider retry path","timeoutSeconds":300}
```

The acknowledgment has `details.schema: "pi.subagent.worker.v1"`, a stable
`workerId`, a carrier `jobId` and `turnId: 1`. A later task uses the same worker:

```json
{"operation":"worker_send","workerId":"worker-…","task":"Use that context to find the cancellation race"}
```

Each accepted send returns a new numbered turn. Busy workers queue follow-ups
in admission order; sends do not steer or interrupt the running task. Keep the
returned turn ID when several callers share a worker. Observe its exact result:

```json
{"operation":"worker_wait","workerId":"worker-…","turnId":2,"timeoutMs":30000}
```

The selected `details.turn` contains its `id`, `status` and `result`. Pending
states are `queued` and `running`; terminal states are `completed`, `failed` and
`cancelled`. A terminal result is the native `ToolOutput`, including the ordinary
subagent result at `details.turn.result.details.results[0]`. Worker results name
`sessionIsolation: "persistent_rpc_worker"` and retain the stable child PID and
hub identity. Omitting `turnId` selects the latest turn once when the observation
begins. New sends cannot retarget an existing wait.

`worker_status` polls without waiting. `worker_wait` accepts 0–600,000 ms and
defaults to 30,000; its timeout returns the pending snapshot and leaves execution
running. A screened completion also enters the owning session's background
follow-up queue. Explicit waits do not consume that queued notice.

```json
{"operation":"worker_close","workerId":"worker-…"}
```

Close cancels the running turn and all queued turns, then retires the process
tree. Repeated close is safe. The carrier job owns the entire worker lifetime,
including idle time: ordinary `jobs wait` waits for worker closure, while
`worker_wait` waits for a task. `jobs cancel`, hub cancellation and session
shutdown also retire the worker. Completed results remain inspectable until
their closed handle is evicted. Existing filesystem effects are not rolled back.

### Limits and supported controls

- Each worker admits at most 32 turns, with at most 64 KiB of task text per turn.
- One original lifetime deadline covers startup, execution, idle time and every
  queued follow-up. The host limit defaults to 900 seconds; `timeoutSeconds` can
  shorten it. Sending another task never renews the deadline.
- Workers share the global eight-job capacity with shell and other native jobs.
  Close idle workers to release their slots.
- Each turn retains at most 128 KiB of encoded native output. A worker retains
  less than 1 MiB of results, reserving space for cancellation/errors. An oversized
  or unprojectable answer becomes an explicit failure with its payload withheld.
  The registry retains at most 32 workers and evicts the oldest closed handle
  when another is admitted; active workers are never evicted.
- Every operation rechecks the current session. Every send captures the current
  privacy policy and known secrets; completed output is screened before retention
  and notices, and observations are screened again under the caller's current
  policy. A canceled or closing job cannot accept another turn or a late result.
- `worker_start` accepts `agent`, `task`, `cwd`, `scope`, `timeoutSeconds` and
  `isolation: "none"`. Follow-ups cannot change frozen launch options. This first
  worker implementation requires Unix nonblocking pipes and refuses worktree
  isolation, output schemas, schema-bearing definitions and historical `seance`
  consultation. Ordinary delegation continues to provide those workflows.

The native regression suite includes a controlled RPC child that remembers a
first prompt in its second response while keeping one PID, queued turn ordering,
the post-turn streaming/compaction barrier, idle cancellation and session ownership.
Rust acceptance remains the DSR quality lane; guest-language diagnostics alone
do not establish native execution correctness.

## Reusable workpools

A native pool owns one background carrier job and 1–8 reusable RPC children.
It accepts further batches while earlier items execute, retains each child's
conversation, and provides one aggregate completion after admission is sealed
and all accepted work drains. The queue and controller live in Rust; an eval
interpreter is not needed to keep them running.

```json
{
  "operation": "pool_start",
  "agent": "reviewer",
  "tasks": [
    "Review parser error handling for concrete correctness defects",
    "Review cancellation and child-process cleanup for concrete races"
  ],
  "concurrency": 2,
  "timeoutSeconds": 300
}
```

The acknowledgment has `details.schema: "pi.subagent.pool.v1"`, a stable
`poolId`, a carrier `jobId`, and `admittedItemIds` in input order. Item IDs are
1-based, never reused and independent of internal worker/turn assignments.
The `items` manifest retains admission order. Internal member worker handles
are not published and cannot be used to cancel a shared carrier accidentally.

Later batches use the existing pool identity:

```json
{"operation":"pool_push","poolId":"pool-…","tasks":["Review session persistence for data-loss risks"]}
```

Each batch is admitted atomically under the carrier's cancellation boundary.
Every assignment and placement is checked before any task is queued: rejection
does not admit a prefix. Idle members take new work before busy members. Among
idle members, assignment prefers the lowest observed context pressure: the
active token estimate divided by the current model's context window, or the
raw estimate when capacity is unknown. The child reports this estimate at its
correlated idle barrier after persistence and compaction, so discarded history
and cumulative provider usage do not inflate the next assignment's load.
`workerContexts` in the manifest exposes each member's `workerIndex`, `tokens`
and `window`; unobserved values are null. A used worker with unknown token load
does not masquerade as an empty conversation. Fresh workers start empty.

When all members are busy, assignment balances pending item counts, using
prior admissions and member order to break ties. Accepted items keep their
assigned child and run in FIFO order. A later item has no guaranteed affinity
to a particular earlier item's child. Failed members are not silently replaced.
This scheduling preserves individual native turns; it does not combine queued
items into OMP's keyed child-yield batches.

### Native operations and result shapes

All controls use the `subagent` tool with an explicit `operation` field:

| Operation | Fields and behavior |
| --- | --- |
| `pool_start` | Requires `agent`; accepts `tasks` (default empty), `concurrency` (default 4), `cwd`, `scope`, `timeoutSeconds`, and `isolation: "none"`. Returns the new identity and admitted item IDs. |
| `pool_push` | Requires `poolId` and a nonempty `tasks` array. Returns all newly admitted item IDs; cannot change launch settings. |
| `pool_status` | Requires `poolId`. Returns one bounded manifest with status, counts, ordered summaries and result references. |
| `pool_result` | Requires `poolId` and `itemId`. Adds the selected item's status and full native result to the manifest. |
| `pool_wait` | Requires `poolId`; accepts `itemId` and `timeoutMs`. Waits for that item, or for terminal pool state when `itemId` is omitted. Does not seal admission. |
| `pool_drain` | Requires `poolId`; accepts `cursor`, `maxItems` (1–4, default 4) and `timeoutMs` (default 0). Returns newly settled full results and a replayable acknowledgment cursor. Does not seal or consume results. |
| `pool_finish` | Requires `poolId`. Seals admission and acknowledges immediately; accepted items continue draining. Repeated finish is safe. |
| `pool_close` | Requires `poolId`. Cancels running and queued work and waits for owned child cleanup. Repeated close is safe. |

Pool status is `open`, `draining`, `completed`, `failed` or `cancelled`.
`sealed` reports whether further native admission is closed. The manifest
includes `concurrency`, `admittedItems`, `queuedItems`, `runningItems`,
`completedItems`, `failedItems` and `remainingMs`. Failed counts include
cancelled items. Each manifest entry has this shape:

```json
{"id":1,"status":"completed","summary":"Bounded task summary…","resultRef":{"poolId":"pool-…","itemId":1}}
```

The summary is an excerpt, not a replacement for the task's native output.
Fetch one exact result with:

```json
{"operation":"pool_result","poolId":"pool-…","itemId":1}
```

This adds `details.itemId` and `details.item: {id, status, result}`. Pending
items have status `queued` or `running` and `result: null`; terminal items have
status `completed`, `failed` or `cancelled` and a non-null native `ToolOutput`.
That output preserves content, native details, error status, child PID and hub
metadata. Accepted observation/control calls remain successful tool calls even
when the selected result failed; inspect `details.item.result.isError` and the
item status. The aggregate carrier result separately reports failed completion.

`pool_wait` accepts a timeout from 0 through 600,000 ms and defaults to 30,000.
Zero polls. Reaching this observation timeout returns a pending snapshot and
does not cancel or restart work. Waiting for the pool without sealing it may
time out even when all currently admitted items have finished: an open pool
still accepts more work.

### Collect completed results while work continues

`pool_drain` returns completed, failed and cancelled items without waiting for
earlier unfinished items. It uses one native observation, with at most four
full results per page:

```json
{"operation":"pool_drain","poolId":"pool-…","maxItems":4,"timeoutMs":10000}
```

The returned `details.schema` is `pi.subagent.pool.drain.v1`. Its `items` are
`{id, status, result}` records in ascending item order among currently settled
results. `result` is the full native `ToolOutput`. The page includes pool/job/
agent identity, pool `status`, `sealed`, and `admittedItems`, `settledItems` and
`pendingItems` counts.

Only advance to `nextCursor` after processing the page:

```json
{
  "operation":"pool_drain",
  "poolId":"pool-…",
  "cursor":{"schema":"pi.subagent.pool.cursor.v1","poolId":"pool-…","itemIds":[2,4]},
  "timeoutMs":10000
}
```

The cursor is an explicit sorted set of acknowledged terminal item IDs. An
omitted cursor starts from an empty set. `cursor` echoes that acknowledgment;
`nextCursor` adds the returned items. Reusing an earlier cursor observes those
results again, along with any newly eligible results that fit the page. Cursors
for another pool, duplicate/unsorted IDs, unadmitted IDs, and acknowledgments of
pending items fail without changing the pool or consuming results.

`hasMore` means further settled results remain beyond this page. `done` means
the pool has terminated, its children have been reaped, and `nextCursor` covers
every admitted item. An open pool is not done simply because its current batch
finished. An empty page with `timedOut: true` means the observation deadline
expired; accepted work continues. Failed/cancelled item evidence remains
available after shutdown, and each read reapplies captured and current privacy
policy. Later pushes retain earlier item IDs and remain compatible with a saved
cursor. The cursor carries no permission and can be used after a kernel reset
only through the owning session's permitted native tool path.

### Finish, cancellation and aggregate delivery

Seal the pool when no more tasks will be added, then wait for its carrier:

```json
{"operation":"pool_finish","poolId":"pool-…"}
```

```json
{"action":"wait","jobId":"job-…","timeoutMs":30000}
```

The second request uses the ordinary `jobs` tool. Successful drain retires all
member processes before the carrier settles. Its final
`result.details.schema: "pi.subagent.pool.v1"` contains the ordered bounded
manifest and result references. Per-item completion notices are suppressed;
the carrier produces one aggregate session completion notice. Explicit result
reads and waits do not consume that notice. Full item results remain available
through `pool_result` while the pool remains retained.

Use `pool_close` or `jobs cancel` to stop work instead of draining it. Either
retires the shared carrier and all its running and queued items. Session
shutdown also cancels owned work. Previously performed filesystem effects are
not rolled back. Normal finish, explicit cancellation and deadline exhaustion
have different outcomes; a cancelled or timed-out carrier may have the jobs
runtime's error result rather than a successful aggregate manifest.

### Bounds, policy and recovery

- Each pool consumes **one of the process's eight active native-job slots**.
  It can own up to eight RPC child processes, including idle members. The
  carrier count is not a child-process count; select concurrency accordingly.
- A pool admits at most 32 items across all pushes. Each assignment is nonempty
  UTF-8 text of at most 64 KiB without NUL. Native `pool_start` accepts an empty
  initial batch; native `pool_push` does not.
- Eval's JavaScript and Python `workpool()` helpers additionally cap the entire
  encoded batch request at 256 KiB and validate it before dispatch. Split larger
  submissions across pushes. Each accepted guest batch uses one host call, as
  does each complete manifest observation.
- One original deadline covers discovery, startup, queueing, execution and idle
  time. The host limit defaults to 900 seconds; `timeoutSeconds` may shorten it.
  Push, finish and observation waits never renew the deadline.
- Children use the definition, model selection, tools and working directory
  captured at startup. Pools currently require Unix and `isolation: "none"`.
  Worktree isolation, output schemas, schema-bearing definitions and historical
  `seance` consultation remain on the ordinary delegation path.
- Every native operation checks the current owning session and cancellation
  state. An eval request also passes its active Agent's current tool, approval,
  plan and extension policy. Captured IDs grant no permission in another
  session or standalone eval. Each accepted batch captures current privacy
  policy; results are screened before storage, aggregate delivery and later
  reads under the caller's current policy.
- Full item output uses the worker limits: at most 128 KiB per item and less
  than 1 MiB per member's retained results, with explicit errors for withheld
  output. Manifests use bounded summaries and result references. The registry
  retains at most 16 pools, evicting an older reaped pool when needed; active
  pools are not evicted. Jobs apply their own aggregate-result retention limits.

Atomic admission does not guarantee that its acknowledgment reaches the caller.
A transport failure, cancellation or interrupted cell can leave an entire batch
accepted even when its reply was lost. Never automatically replay such a batch.
When `poolId` is known, inspect `pool_status` to recover the actual admitted
items. The guest helper closes its own admission after an ambiguous mutation,
preserves known handles, and can reconcile them with `peek()`; finishing or
cancelling remains explicit.

Kernel reset loses guest variables without cancelling native jobs. Preserve
both IDs outside the interpreter, or recover them from the owning session's
`jobs list`: the carrier label is `reusable subagent pool <poolId>`, and the
snapshot supplies its job ID. Use native `pool_status`, `pool_result`,
`pool_finish` or `pool_close` after recovery. A guest that never received its
first acknowledgment reports `unknownAdmission: true` and cannot claim complete
cleanup for an unidentified carrier. Recovery uses the direct native operations;
the original unacknowledged guest object does not automatically bind to the
recovered identity. This retention is
bounded and process-local; it does not provide disk durability, process-restart
resume or reconstruction of child conversations from job artifacts.

See [eval workpool examples](eval-structured-results.md#reusable-workpools) for
JavaScript and Python handles, aggregate jobs and mixed waits. Native regression
coverage lives in `src/subagents/worker/pool.rs`; the authoritative acceptance
entry point remains `dsr quality --tool pi_agent_rust`.
