# Structured results inside Python and JavaScript eval

`await tool.result(name, args)` executes one call through eval's native bridge
and returns a JSON object rather than flattened text. In an Agent, the bridge
uses that Agent's enabled tools and normal execution policy:

```javascript
const result = await tool.result('read', {path: 'plot.png'});
const image = result.content.find(block => block.type === 'image');
if (image) display(image.data, image.mimeType);
```

The envelope has `schema: "pi.eval.tool_result.v1"`, native `content` blocks,
`details` (including null, arrays and scalar values), and boolean `isError`.
A returned native tool error, approval denial, plan rejection or extension
veto is data with `isError: true`. Bridge availability, recursion, ownership,
input, transport and serialization failures reject the promise. There is no retry.
Images are carried as native MIME/base64 blocks, not fetched or displayed
automatically. Explicit display still applies its own validation and budgets.

Python exposes the same envelope as an ordinary dictionary:

```python
result = await tool.result("read", {"path": "data.txt"})
if not result["isError"]:
    print(result["content"][0]["text"])
```

Python requires both the name and argument dictionary. A native unsuccessful
`ToolOutput` is returned with `isError: True`; bridge and decoding failures
raise `ToolBridgeError`. Concurrent calls share the existing serialized async
bridge. Cancelling a queued call sends nothing; cancelling a dispatched call
drains its reply before raising cancellation, without retrying it. Cell-end
cleanup also drains dispatched work. The original host deadline bounds a
stalled exchange, and already performed effects are not rolled back.

The complete encoded envelope is limited to 4 MiB during serialization;
details are also limited to 64 levels and 131,072 JSON values. An oversized
result fails explicitly instead of returning an apparently successful fragment.
The existing `tool.call` returns text and rejects native errors as before, with
an explicit 1 MiB text limit. Cells may retain returned objects in their normal
namespace. Requests still use the original session owner and cell deadline.

Both helpers are available in session and per-call kernel modes. The original
Python `tool.call` and synchronous reader helpers continue to return text.

`__pi_tool_result` is the internal protocol operation. Its object requires
exactly `name` and object-valued `args`; the inner name passes the ordinary
tool policy, with no recursive unwrapping or permission fallback.

## Agent-scoped tool orchestration

An Agent-owned cell can call any currently mounted native, extension or MCP
tool by its actual name, including discoverable tools. For example, when `write`
and `bash` are enabled:

```javascript
await tool.call("write", {path: "report.txt", content: "Checks completed.\n"});
const check = await tool.result("bash", {command: "git diff --check"});
if (check.isError) throw new Error(check.content[0].text);
```

Each invocation passes through the owning Agent's plan gate, approval handler,
extension call/result hooks, cancellation and secrets transformations. Enabling
eval does not enable any other tool. A disabled tool fails with
`EVAL_BRIDGE_UNAVAILABLE`; there is no standalone-reader fallback inside an
Agent. Calling an opt-in tool such as `subagent` therefore requires that tool to
be mounted and the ordinary approval policy to permit its request.

There are at most **64 attempted host tool calls per cell**, including denied
calls. Each receives a fresh native tool-call ID and the normal start, update
and end events. All calls, including time spent awaiting approval, use the
cell's existing deadline. The native caller captures the session identity before
queueing, checks it at the first bridge lookup, and checks it again immediately
before native dispatch after approval and hooks. A host cannot follow a cell
into a replacement session. Cancelled calls are not retried or reported as
successful; effects already performed by a tool are not rolled back.

`eval`, `xdev` and nested internal protocol wrappers are refused with
`EVAL_BRIDGE_RECURSION`. Call a discoverable tool by its own name. This avoids
reacquiring the same kernel lease or obtaining a fresh host budget through a
wrapper. The host is borrowed only by the active invocation, never retained in
the kernel or forwarded to a nested tool. Native calls are serialized, but an
explicit background subagent launch returns while its owned task keeps running.
The retained helpers below use this same bridge for launch and management.

Native result details include `toolCalls` with schema `pi.eval.tool_calls.v1`.
Each entry records `toolCallId`, `tool`, and `status` (`completed`, `failed` or
`incomplete`); completed/failed results also include `isError`. Receipts contain
no tool arguments or output bodies. They survive caught guest errors and
partial cells. An incomplete or failed call may have performed effects; the
status is not a rollback guarantee. Python hard failures include the receipts
in the error's `EVAL_TOOL_CALLS` field.

Direct standalone `EvalTool::execute` embeddings still expose only local
`read`, `grep`, `find` and `ls`. They do not retain authority from an earlier
Agent call. Typed model judgments use the separate explicit `__pi_judge` and
`__pi_judge_batch` operations with their own selection and privacy policies.
Single judgments and batch items share a 16-reserved-attempt limit.

## Retained background agent jobs

Agent-owned cells can launch native subagents and retain their job handles for
later cells. Both `subagent` and `jobs` must be enabled on the owning Agent.
The launch passes the ordinary subagent approval and plan policy; every later
status, wait or cancellation request passes the current policy for `jobs`.
There is no separate worker registry or retained permission grant.

For example, launch two independent tasks in JavaScript:

```javascript
globalThis.work = [];
work.push(await agent('Inspect the parser for correctness issues.', {agent: 'task'}));
work.push(await agent('Inspect the tests for missing failure cases.', {agent: 'task'}));
work.map(handle => handle.id);
```

The native jobs run concurrently after their individual launch calls return.
Use `globalThis` for handles that must survive JavaScript's top-level-await
cell. In a later cell, collect their complete native results in input order:

```javascript
const results = await wait(work, {timeout: 20, raiseErrors: false});
for (const result of results) {
    console.log(result.content.filter(block => block.type === 'text')
        .map(block => block.text).join('\n'));
}
```

Python retains ordinary namespace assignments and supports the same workflow:

```python
work = [
    await agent('Inspect the parser for correctness issues.', agent='task'),
    await agent('Inspect the tests for missing failure cases.', agent='task'),
]
```

```python
results = await wait(work, timeout=20, raise_errors=False)
for result in results:
    print('\n'.join(block['text'] for block in result['content']
                    if block['type'] == 'text'))
```

The returned handle exposes `id` and the selected `agent` name. Its methods
are asynchronous in both languages:

| Method | Result |
| --- | --- |
| `status()` | Current native job snapshot, using a zero-duration wait. |
| `done()` | Whether the job has settled. |
| `output()` | Settled native `ToolOutput`, or null/`None` while running. |
| `wait(seconds=30)` | Settled native `ToolOutput`; raises on failure or timeout. |
| `cancel()` | Native snapshot after cancellation and owned child cleanup. |

In Python, `await handle` also waits with the default 30-second budget.
`wait(handles, ...)` accepts up to 32 handles and applies one shared timeout
across the collection. The jobs are already running; waiting in input order
does not serialize their execution. `raiseErrors: false` / `raise_errors=False`
returns failed native outputs as data. It does not suppress a timeout, host
denial, missing job or malformed reply.

Wait timeouts accept finite numeric seconds from 0 through 600. A zero wait
only polls. A timeout leaves the job running and available through its handle
or the ordinary `jobs` tool. Waiting uses the current cell's watchdog, so set
the eval cell timeout longer than the requested wait when appropriate. A
kernel reset loses guest variables; it does not cancel the native jobs, which
remain discoverable through `jobs list`. Session shutdown cancels owned jobs.

Launch options map directly to native subagent fields. JavaScript accepts
`agent`, `cwd`, `isolation`, `isoApply`, `outputSchema`, `schemaMode`,
`timeoutSeconds` and `scope`; Python uses `iso_apply`, `output_schema`,
`schema_mode` and `timeout_seconds` for the corresponding compound names.
The default agent is `task`; definitions must already be discoverable by the
native subagent tool. Unknown options are rejected before a host call.

`outputSchema` / `output_schema` and strict schema mode preserve native typed
results: access `result.details.results[0].data` in JavaScript or
`result['details']['results'][0]['data']` in Python. Native content blocks,
errors, hub references and isolation metadata remain available in the same
output. Successful native outputs may omit `isError`; use `result.isError`
or `result.get('isError', False)` when checking it.

The shared jobs limits apply: eight active jobs across bash and host tasks
in the process, a 1 MiB encoded final-result limit and a 32 MiB retained host-result
budget. A result that cannot be safely protected or retained is withheld as
an explicit error. The subagent's definition, model and execution deadline
are captured at launch. Its final result is privacy-protected before job
retention or completion delivery. See [native background delegation](subagent-builtins.md#background-delegation)
for cancellation, isolation and lifecycle details.

The default `agent()` call creates an independent task. Set `keepAlive: true`
in JavaScript or `keep_alive=True` in Python for the reusable conversation
described below. Waiting does not consume the normal session completion notice
or pause the eval watchdog. Each handle operation still counts toward the
active cell's 64 host-call limit; possession of an old handle never supplies
authority to a standalone eval invocation or a replacement session.

## Reusable agents and queued turns

A reusable agent retains one native RPC child and its conversation across
prompts. This lets an agent inspect a subsystem, then implement a follow-up
using that context. Enable `subagent` on the owning Agent; launch, send,
inspection, waiting and cancellation all use its current tool policy.

```javascript
globalThis.worker = await agent('Inspect the parser and identify the main bug.', {
    agent: 'task', keepAlive: true, timeoutSeconds: 300,
});
```

In a later cell, queue the next turn and retain its own handle:

```javascript
globalThis.fix = await worker.send('Implement the correction and explain the change.');
await fix.wait(20);
```

Python retains the corresponding objects in its ordinary namespace:

```python
worker = await agent('Inspect the parser and identify the main bug.',
                     agent='task', keep_alive=True, timeout_seconds=300)
fix = await worker.send('Implement the correction and explain the change.')
result = await fix.wait(20)
```

Sending while the child is busy appends to its native FIFO queue. Each turn
starts after the preceding turn finishes and the child's streaming and
compaction have settled. The child keeps the same process, definition, model,
tools and working directory for its lifetime.

The worker exposes `id` / `workerId`, `jobId`, `agent` and `turnId` in JavaScript;
Python uses `id` / `worker_id`, `job_id`, `agent` and `turn_id`. A returned turn
handle has its own immutable worker and turn selection. The worker's selected
turn advances to its latest acknowledged send; an earlier turn handle keeps
pointing to its original result.

| Method | Reusable worker or turn behavior |
| --- | --- |
| `worker.send(prompt)` | Queue a follow-up and return its fixed turn handle. |
| `status()` | Native worker snapshot with the selected turn's status and result. |
| `done()` | Whether the selected turn has completed, failed or been cancelled. |
| `output()` | Selected turn's native `ToolOutput`, or null/`None` while pending. |
| `wait(seconds=30)` | Wait for the selected turn; raise on failure or timeout. |
| `close()` / `cancel()` | Close the entire worker, cancelling its running and queued turns. |

Closing a turn handle also closes its entire worker. There is no individual
turn cancellation. Successful turns leave the worker idle and available for
another send until it is closed or reaches its original deadline.

`wait()` accepts a mixture of one-shot jobs, workers and fixed turn handles.
It captures each worker's current turn before starting the wait, preserves
input order, and shares one timeout across the collection. Python also accepts
`await worker` and `await fix`. The returned result preserves native content
and `details.results[0]`, including the child PID, hub identity and
`sessionIsolation: "persistent_rpc_worker"`.

Reusable workers are currently supported on Unix and accept `agent`, `cwd`,
`scope`, `timeoutSeconds` / `timeout_seconds`, and `isolation: "none"`.
Worktree isolation, output schemas and `isoApply` / `iso_apply` remain on the
independent-task path. A schema-bearing agent definition cannot be reused as
a worker. Invalid combinations are rejected before a worker starts.

Each worker admits at most 32 turns, with at most 64 KiB of UTF-8 text per
prompt and no NUL characters. Its fixed lifetime includes startup, idle time,
and all queued turns. The native host limit defaults to 900 seconds; a supplied
timeout can shorten that limit. Idle workers still occupy one of the process's
eight active job slots. Retained output is bounded to 128 KiB per turn and
less than 1 MiB per worker; oversized or unsafe output becomes an explicit
error. Each send captures current privacy policy, and output is screened again
when read. See [native reusable workers](subagent-builtins.md#reusable-workers)
for the direct tool protocol and ownership rules.

Call `await worker.close()` after the conversation is finished. Kernel reset
loses its guest handle, while the native carrier remains discoverable through
`jobs list`. Cancelling that job or shutting down the owning session retires
the worker and its queued turns.

## Reusable workpools

`workpool()` creates a native pool with **one carrier job**, a bounded queue
and reusable RPC child agents. Native actors keep executing after the eval
cell returns; no guest timer, callback or retained tool permission drives the
queue. Items assigned to the same child share its conversation, and each child
executes its turns in FIFO order. Assignment balances pending work across the
children; a later prompt is not guaranteed to reach a particular earlier child.

```javascript
globalThis.pool = await workpool({
    agent: 'reviewer', concurrency: 2, name: 'parser-review', timeoutSeconds: 300,
});
globalThis.items = await pool.push([
    'Review parser error handling and identify concrete correctness defects.',
    'Review cancellation and child-process cleanup for concrete races.',
]);
console.log({poolId: pool.poolId, jobId: pool.jobId});
```

In a later cell, add work, inspect the bounded manifest, and seal admission:

```javascript
await pool.push(['Review session persistence for data-loss risks.']);
console.log((await pool.peek()).items);
globalThis.reviewJob = await pool.finish();
```

`finish()` lets all accepted items drain and retires the child processes. Its
returned job receives one ordered aggregate result after that cleanup. Collect
the manifest and an individual full result in another cell:

```javascript
const [aggregate] = await wait([reviewJob], {timeout: 20, raiseErrors: false});
console.log(aggregate.details);
const first = await items[0].output();
console.log(first.content);
```

Python exposes the same lifecycle and retains ordinary namespace assignments:

```python
pool = await workpool(agent='reviewer', concurrency=2,
                      name='parser-review', timeout_seconds=300)
items = await pool.push([
    'Review parser error handling and identify concrete correctness defects.',
    'Review cancellation and child-process cleanup for concrete races.',
])
print(pool.pool_id, pool.job_id)
```

```python
await pool.push(['Review session persistence for data-loss risks.'])
snapshot = await pool.peek()
review_job = await pool.finish()
```

```python
aggregate, = await wait([review_job], timeout=20, raise_errors=False)
print(aggregate['details'])
first = await items[0].output()
print(first['content'])
```

The pool exposes `id` / `poolId` and `jobId` in JavaScript, or `id` / `pool_id`
and `job_id` in Python. IDs are null/`None` until the first native admission.
`pool.items` retains fixed item handles in admission order as a frozen array or
Python tuple. Each item has `itemId` / `item_id`, `poolId` / `pool_id`, `jobId` /
`job_id`, and `agent`; `id` is the item ID. Items expose `status()`, `done()`,
`output()` and `wait(seconds=30)`. They expose no worker ID or cancellation
method: cancellation belongs to the entire pool.

| Operation | Result |
| --- | --- |
| `push(tasks)` | Atomically admit a batch and return its fixed item handles in input order. |
| `peek()` / `status()` | One native manifest: pool state, counts, ordered item summaries and result references. |
| `finish()` | Seal admission and return the aggregate `AgentJob`; accepted work continues draining. |
| `wait(seconds=30)` | Seal admission, then await the aggregate native `ToolOutput`; raise on failure or timeout. |
| `output()` | Poll the aggregate job result without sealing; null/`None` while pending. |
| `close()` / `cancel()` | Seal admission, cancel running and queued work, reap children, and report cleanup. |

Pool states are `open`, `draining`, `completed`, `failed` and `cancelled`.
Item states are `queued`, `running`, `completed`, `failed` and `cancelled`.
The manifest's `failedItems` count includes cancelled items. Summaries are
bounded excerpts with `resultRef: {poolId, itemId}`; they are not full task
results. `item.output()` retrieves the selected native `ToolOutput`, or
null/`None` while pending. `item.wait()` waits for that item without sealing
the pool. Global `wait()` accepts a mixture of pool items, one-shot jobs,
reusable workers and fixed worker turns, preserving input order and one shared
timeout. Python also supports `await item` and `await pool`.

Creation is lazy: `workpool()` makes no native call until a nonempty `push()`
or `finish()`. Closing an unused pool starts nothing; finishing an unused pool
creates and seals a real empty carrier job. An open pool remains available
after its current batch drains. Call `finish()` when no further work will be
added, or `close()` to stop it. Repeated acknowledged finish/close calls do not
replay their native mutation. Cancelling does not roll back filesystem effects.

### Bounds, admission and ownership

Concurrency defaults to four and accepts 1–8 children. Each pool occupies
**one of the process's eight active native-job slots**, while up to eight RPC
child processes belong to that carrier. Idle children still use resources;
one job slot does not mean one child process. All children share the original
pool deadline, covering discovery, startup, execution, queueing and idle time.
The host limit defaults to 900 seconds; an explicit timeout can shorten it.
Neither a later push nor a wait renews that deadline.

A pool admits at most 32 items over its lifetime. Each prompt must be nonempty
UTF-8 text of at most 64 KiB without NUL. JavaScript accepts an array; Python
accepts a list or tuple. Empty pushes on an open pool are local no-ops. The
entire encoded guest batch request is limited to **256 KiB** in both languages;
split larger submissions into smaller pushes. All text, capacity and encoded
size checks happen before dispatch. A nonempty push, `peek()`, or `status()`
uses one host call, not one call per item. The normal 64-call cell allowance
and eval watchdog still apply, and pool commands **must be awaited**.

Native batch admission is atomic: a rejected batch does not enqueue a prefix.
That does not make receipt of the acknowledgment atomic with admission. A
lost reply or interrupted cell can leave a whole batch admitted but unknown
to the guest. The helper closes further guest admission, keeps previously
acknowledged handles, and never automatically retries. The exception's
`admitted` field contains retained acknowledged handles and `requested` records
the attempted batch size. When the pool ID is known, `peek()` reconciles the
actual native items and updates `pool.items`; it does not reopen guest admission.
Use `finish()` to drain them or `close()` to cancel them.

If the first acknowledgment is lost, the guest may never receive the pool ID.
It reports `unknownAdmission: true`; `close()` distinguishes `allKnownClosed`
from `allClosed` and cannot claim complete cleanup while an unknown carrier
may remain. Inspect the owning session's `jobs list`: a carrier's label includes
`reusable subagent pool <poolId>`, alongside its job ID. Use those identities
with native `pool_status`, `pool_finish` or `pool_close`, or cancel the carrier
through `jobs`. Recovery uses those direct native operations; an unacknowledged
guest object does not automatically bind to a recovered identity. No batch is
replayed during recovery.

Pool options are `name`, `agent`, `concurrency`, `cwd`, `scope`,
`isolation: "none"`, and `timeoutSeconds` / `timeout_seconds`. Names are labels
for the guest object. Agent definition, model, child tools and working directory
are captured at native startup and cannot be changed by later pushes. Pools
currently require Unix, refuse worktree isolation and output schemas, and do
not support schema-bearing definitions or historical `seance` consultation.

Every native launch, control request and result read re-enters the active
Agent's current policy. Pool operations need `subagent`; aggregate job methods
need `jobs`. A descriptor never carries approval or authority into standalone
eval or another session. Admission checks the owner and cancellation state;
accepted output is privacy-screened before retention, and reads apply the
caller's current privacy policy. Session shutdown cancels the owned carrier
and its children. These are child-agent jobs, not detached general eval cells.

Kernel reset loses guest objects but leaves native jobs running. Retain both
IDs outside the kernel or recover them through `jobs list`, then use the direct
[native pool operations](subagent-builtins.md#reusable-workpools). The native
pool registry retains at most 16 pools and may evict an older reaped pool when
another is admitted. Full item results use the bounded reusable-worker result
storage; the aggregate job retains the ordered summary manifest. This is
process-local bounded retention, not a disk-backed queue, process-restart
recovery or durable conversation storage.

Native aggregate completion is delivered through the existing session job
notice, after drain and child cleanup. OMP's context-load scheduling, keyed
yield batches and custom child-defined eval tools remain separate work.

## Validation

`tests/eval_structured_reader.test.cjs` exercises the actual embedded JS factory
with a controlled host. It is diagnostic coverage, not a native integration
claim. `tests/eval_structured_reader_test.py` exercises the actual Python kernel
in subprocesses with a controlled protocol host, including queued/active
cancellation, reply draining and retained cells. It does not execute Rust
tools. `src/eval/bridge_result.rs` includes native reader, Python and QuickJS
regressions, plus serialization, malformed-input and whitelist tests. Its
retained-agent regression launches a real native child from both kernels,
checks typed output in a later cell, and rejects a retained handle when that
cell has no Agent host. The guest diagnostics also cover fixed warm turns,
mixed waits, real protocol field names, atomic pool batches, aggregate finish,
lost-acknowledgment reconciliation, interrupted-cell recovery and ambiguous
cleanup reporting. Native worker
regressions in `src/subagents/execution/worker.rs`, `src/subagents/worker.rs`
and `src/jobs/host_task.rs` cover same-process conversation reuse, compaction
barriers, cancellation/reaping, privacy projection and session ownership. The
native pool regressions in `src/subagents/worker/pool.rs` exercise atomic
admission, retained item results, aggregate drain and carrier cancellation. The
`eval_host_*` tests in `src/agent_tests.rs` exercise the actual Agent and kernels,
including native writes and reads, approvals, live plan changes, extension
hooks, disabled tools, recursive wrappers, call budgets and session changes.
The authoritative acceptance entry point remains
`dsr quality --tool pi_agent_rust`.
