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
Completions, single judgments and batch items share a 16-reserved-attempt limit,
including the background model requests below.

## Retained background model requests

Agent-owned cells can start a stateless model request and collect its answer in
a later cell. Supply the launching eval call's explicit
`judge_model: {provider, model}`, `judge_model: {role: 'smol'}`, or use an SDK
host-bound Judge. The helpers
return after native admission; they do not await the model's final answer.

JavaScript:

```javascript
globalThis.extraction = await completion(
  'Extract the paths from: src/api.rs and src/ui.rs need review.',
  {schema: {type: 'array', items: {type: 'string'}}, timeoutSeconds: 60}
);
globalThis.review = await judgeBatch(
  {api: {change: 'adds input validation'}, ui: {change: 'adds regression tests'}},
  {safe: {type: 'bool', instructions: 'Does the change improve robustness?'}},
  {concurrency: 2}
);
display({extraction: extraction.id, review: review.id});
```

A later cell can mix retained model and agent handles in the existing `wait`:

```javascript
const [paths, judgments] = await wait([extraction, review], {timeout: 20});
display({paths, items: judgments.items});
```

Python:

```python
extraction = await completion(
    'Extract the paths from: src/api.rs and src/ui.rs need review.',
    schema={'type': 'array', 'items': {'type': 'string'}},
    timeout_seconds=60,
)
review = await judge_batch(
    {'api': {'change': 'adds input validation'}, 'ui': {'change': 'adds tests'}},
    {'safe': {'type': 'bool', 'instructions': 'Does this improve robustness?'}},
    concurrency=2,
)
# In a later cell:
paths, judgments = await wait([extraction, review], timeout=20)
display({'paths': paths, 'items': judgments['items']})
```

| Helper | Options | Successful `wait()` value |
|---|---|---|
| `completion(prompt, options)` / `completion(prompt, **options)` | `model` role, `system`, `schema`, `images`, `maxTokens` / `max_tokens`, `timeoutSeconds` / `timeout_seconds` | Text, or validated `data` when a schema was requested |
| `judge(state, questions, options)` / `judge(state, questions, **options)` | `model` role, `images`, task timeout | The typed `answers` object |
| `judgeBatch(states, questions, options)` / `judge_batch(states, questions, **options)` | `model` role, `images`, `concurrency`, `retries`, `minOk` / `min_ok`, task timeout | The complete ordered `pi.eval.judgment-batch.v1` aggregate |

The factories are asynchronous: **await the factory to get a handle**. Options
use an object in JavaScript and keyword arguments in Python. Unknown options,
including `intent`, are rejected. Requests use the
launching cell's native binding unless they explicitly select a configured
model role as described below, with no model fallback. Single completions and
judgments make one attempt; retained batches can explicitly enable bounded retries.
Completion and
judgment images use the existing path or embedded-image
format; batch images map literal state keys or canonical array indices to
attachment lists. Image reads finish through the current Agent's enabled `read`
tool before background provider admission. The selected model must declare
image support, including when a completion also supplies a JSON Schema. Text
privacy screening does not redact pixels or perform OCR.

### Retained batch retries and minimum successful items

`retries` accepts an integer from 0 through 3, defaulting to **0**. `minOk` in
JavaScript and `min_ok` in Python accept an integer from 0 through the number
of items, defaulting to **all items**. These preserve Pi's existing cost and
failure behavior; OMP v18.12.6 defaults to one retry and one successful item.
For example, Python `await judge_batch(states, questions, retries=1, min_ok=2)`
allows one extra attempt per failed item and requires two successful items.

Admission reserves `item_count * (retries + 1)` of the launching eval cell's
shared 16 model attempts before any host call, image read or provider request.
An over-budget batch is rejected in full. Unused reservations cannot be spent
by later calls in that cell. Retries use the same screened evidence, selected
model, credentials, privacy snapshot and original native job deadline.

Only malformed typed answers and classified transient provider failures can
retry. Classification happens before diagnostics are sanitized. Authentication,
access, policy, invalid-request, cancellation and timeout failures do not retry.
Retries queue behind untouched peers; successful items never rerun. Every
attempt retains its own receipt with `item`, `itemAttempt` and, once settled,
`retryScheduled`; provider diagnostic bodies remain withheld.

`minOk` is a success threshold, not early termination: every original item still
settles. The aggregate includes `minOk`, `retries` and `meetsMinimum`.
`allOk` continues to mean every item succeeded, so a batch can have
`meetsMinimum: true`, `allOk: false` and real failed items. Ordinary `wait()`
returns that aggregate when the threshold is met; falling short raises with
the native result attached. A threshold of zero explicitly accepts all-failed
batches, but cannot turn cancellation or timeout into successful completion.

### Per-request model roles

A cell can route cheap extraction and deeper review to different configured
models. Admit model work explicitly with `judge_model: {role: 'smol'}` on the
eval invocation, then choose a role on each request:

```javascript
globalThis.extraction = await completion('Extract the changed file paths.', {
  model: 'smol', schema: {type: 'array', items: {type: 'string'}}
});
globalThis.review = await judge(
  {change: 'adds input validation'},
  {safe: {type: 'bool', instructions: 'Does this prevent invalid input?'}},
  {model: 'slow'}
);
```

Python accepts `await completion(prompt, model='smol', schema=...)`,
`await judge(state, questions, model='slow')`, and
`await judge_batch(states, questions, model='slow')`. A batch uses one selected
role for all its items. Omitting the helper's `model` uses the cell's original
`judge_model`; a previous request's override never changes that default. A
cell admitted with `{provider, model}` can also select a configured helper role.

Accepted role names are exactly `default`, `smol`, `slow`, `plan`, `commit`,
`vision`, `designer`, `task`, `advisor`, and `tiny`. Each must be explicitly
configured in the trusted native settings' `modelRoles` object, using a
registered model spec such as `provider/model` or `provider/model:high`.
The configured thinking suffix is clamped to the selected model's supported
levels. Provider endpoints, credentials, request headers and image capability
come from the host catalog and authentication store. Global and matching
workspace-scoped disabled-provider settings apply to both role and named
selections.

Eval deliberately requires an explicit role assignment: an unset role does
not inherit another role, legacy defaults, the current conversation model or
CLI role flags. An unknown, disabled or uncredentialed destination fails with
a sanitized binding error; it never selects a substitute model. Helper `model`
values are role names only, not arbitrary provider/model strings, endpoints,
credentials or thinking overrides. A host-bound SDK `Judge` remains fixed and
rejects helper role overrides.

Each distinct selection resolves once on first use in the current cell. The
native job captures that concrete provider, credentials, thinking level and
privacy snapshot; later cells and settings changes do not retarget accepted
work. The launch receipt records `selectedModel`, and successful results retain
the actual `provider` and `modelId`. All roles share the existing 16-attempt
cell limit, including every possible retained batch attempt. A helper role alone does not
authorize networking: every cell launching model work still needs its own
explicit `judge_model` or a host-bound Judge. Reading an existing job does not
need a new model selection.

The foreground raw operations `__pi_complete`, `__pi_judge` and
`__pi_judge_batch`, and the legacy `__pi_complete_start`, accept the same
optional `model` role field under these rules.

### Retained handle operations

| Handle operation | Behavior |
|---|---|
| `id`, `jobId` / `job_id`, `operation` | Native job identity and immutable operation kind in JavaScript; read-only public properties in Python |
| `status()` | Current native job snapshot, including `status` and any published result or batch progress |
| `done()` | Whether native status has settled |
| `output()` | Latest native `ToolOutput`, including batch progress or preserved interruption evidence, or null/None before any output exists |
| `progress()` | Batch handles only: ordered item states, counts, attempt receipts and `jobStatus`, or null/None when no valid progress was retained |
| `drain({cursor, timeout})` | JavaScript batch handles only: native page of settled items not acknowledged by the supplied cursor; default timeout 30 seconds |
| `wait(timeout)` | Wait up to the given seconds, then return the typed value above; default 30 seconds |
| `wait(timeout, false)` / `wait(timeout, raise_errors=False)` | Preserve a completed mixed batch, or return the native failure output when no typed payload exists |
| `cancel()` | Cancel through the current cell's `jobs` authority and return the native snapshot |

Ordinary `wait()` raises on a failed, cancelled or timed-out native job. The
error retains its native output as `error.result`; JavaScript also exposes
`error.jobId` and Python `error.job_id`. For a mixed-success batch, use
`output()` or the non-raising wait to inspect both successful and failed items
when the requested `minOk` threshold was not met.
A wait that expires while the job is still running raises `TimeoutError` and
leaves the job running. Repeated waits observe the same job and do not repeat
provider work. Global `wait()` preserves input order, accepts these handles
alongside existing agent/turn/pool-item handles, and uses one shared timeout.

For a retained batch, `await review.progress()` observes all original items in
input order. Each item has its original `key` and a `status` of `queued`,
`running`, `completed` or `failed`. Only completed items carry a typed `verdict`;
failed items carry a sanitized `error`. The view includes `total`, `completed`,
`succeeded`, `failed`, `queued`, `running`, `retries`, `minOk`, `meetsMinimum`
and native `judgments` receipts. A failed attempt that will retry leaves the
item queued or running: it is not counted as settled or delivered by `drain()`.
Receipt count can exceed item count while staying within the reserved attempt budget.
Observations do not consume items and can be repeated after attaching by job ID.

The view's `jobStatus` comes from the same native snapshot as its evidence.
`complete: true` means a final aggregate exists, including a mixed-success
aggregate; inspect `failed` before treating the whole batch as successful.
Cancellation and task timeout preserve the most recent screened progress with
`complete: false` and `jobStatus: 'killed'` or `'timedOut'`. Pending counts in
that retained view describe the last update; the outer job has stopped.
A missing first update or failure without preserved progress returns null/None.
A running batch still raises `TimeoutError` when its wait expires, even if some
items already have verdicts. Non-raising waits on an interrupted batch return
the full native failure output with its retained evidence.

Native running output wraps the partial `ToolOutput` in
`pi.host_job.progress.v1`; cancellation and timeout use
`pi.host_job.interrupted.v1`. Both carry `partialResult`, whose details have
schema `pi.eval.model-job.progress.v1`. `progress()` projects that inner view
and adds `jobStatus`; `status()` and `output()` keep the complete native wrappers.

After a kernel reset, recover a job ID from `jobs {action: 'list'}` and attach:

```javascript
globalThis.extraction = await completion.attach('known-job-id');
```

Python uses `await completion.attach('known-job-id')`; the same form exists on
`judge` and `judgeBatch` / `judge_batch`. Attachment checks current-session
access and the native operation label. It starts no model work and requires
no new `judge_model` selection. Every status, output, wait and cancel operation
uses the current cell's host, so a retained guest handle is never a retained
permission grant. An unavailable, foreign-session or wrong-kind job fails.

All inputs, image reads, privacy checks and schema checks complete before the
provider task is admitted. The owned task retains no interpreter or borrowed
Agent tool host. Kernel reset and `per_call` retirement do not stop an accepted
job. Session shutdown and original Agent cancellation do; a process restart
does not resume jobs. An invalid or lost start acknowledgment is never
automatically retried: inspect `jobs list` before explicitly starting again.

The helpers use [native retained model jobs](judgment.md#retained-background-model-jobs),
including the shared 16-attempt cell budget, 1–16-item batches, concurrency up
to eight, bounded task timeouts, final-result limits and ordinary job retention.
Batch progress is a bounded snapshot of all items. JavaScript batch `drain()`
uses the native waiter and an explicit cursor: process a returned page before
passing its `cursor` to the next call. There is no hidden consuming cursor;
reusing the same cursor preserves replayable evidence, including interruption
results. Python can use the raw `__pi_judge_batch_drain` bridge with `jobId`,
`cursor` and `timeoutMs`. If output privacy would change a typed answer or schema-validated
data, the native job fails without retaining a misleading structured success.
Progress and final outputs pass through the current host's read policy.

The completion helper uses `__pi_model_complete_start`, with the uniform
`pi.eval.model-job.v1` acknowledgment and `pi.eval.model-job.result.v1` final
envelope shared by these handles. Its native job label is `eval model completion`.
The existing [raw `__pi_complete_start` operation](eval-background-completions.md)
continues to return `pi.completion.background.v1` and direct `pi.completion.v1`
result details under the label `eval completion`. Inspect those raw jobs through
`tool.result('jobs', ...)`; `completion.attach()` deliberately rejects their
different result contract. Both entry points use the same session-owned jobs
runtime and capacity limits.

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
executes its turns in FIFO order. Idle children with lower observed context
usage receive new assignments first. The token estimate and model capacity
come from the child's post-turn, post-compaction idle barrier; busy children
keep their existing balanced FIFO queues. A later prompt is not guaranteed to
reach a particular earlier child.

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
| `drain({cursor, timeout, maxItems})` / `drain(cursor=None, timeout=0, max_items=4)` | One native page of previously unacknowledged completed, failed or cancelled item results. Does not seal admission. |
| `finish()` | Seal admission and return the aggregate `AgentJob`; accepted work continues draining. |
| `wait(seconds=30)` | Seal admission, then await the aggregate native `ToolOutput`; raise on failure or timeout. |
| `output()` | Poll the aggregate job result without sealing; null/`None` while pending. |
| `close()` / `cancel()` | Seal admission, cancel running and queued work, reap children, and report cleanup. |

Pool states are `open`, `draining`, `completed`, `failed` and `cancelled`.
Item states are `queued`, `running`, `completed`, `failed` and `cancelled`.
The manifest's `failedItems` count includes cancelled items. Summaries are
bounded excerpts with `resultRef: {poolId, itemId}`; they are not full task
results. Its `workerContexts` rows contain a one-based `workerIndex`, `tokens`
and `window`; null values mean the child has not supplied that measurement.
These are the latest observed context estimates, not provider billing usage or
a live measurement of a running prompt. `item.output()` retrieves the selected
native `ToolOutput`, or null/`None` while pending. `item.wait()` waits for that item without sealing
the pool. Global `wait()` accepts a mixture of pool items, one-shot jobs,
reusable workers and fixed worker turns, preserving input order and one shared
timeout. Python also supports `await item` and `await pool`.

Creation is lazy: `workpool()` makes no native call until a nonempty `push()`
or `finish()`. Closing an unused pool starts nothing; finishing an unused pool
creates and seals a real empty carrier job. An open pool remains available
after its current batch drains. Call `finish()` when no further work will be
added, or `close()` to stop it. Repeated acknowledged finish/close calls do not
replay their native mutation. Cancelling does not roll back filesystem effects.

### Collect completed pool items across cells

Use `drain()` to collect full results as items finish, including while other
items remain queued or running. Each call uses one native operation and returns
at most four items, ordered by item ID within the page. A slow earlier item
does not block a later completed item. The cursor records the acknowledged
IDs explicitly, so that earlier item is still collected when it eventually
finishes.

After the pool's first nonempty `push()`, initialize a cursor once:

```javascript
globalThis.poolCursor = null;
```

In a later cell, process a page and then acknowledge it:

```javascript
const page = await pool.drain({cursor: poolCursor, timeout: 20, maxItems: 4});
for (const item of page.items) {
    console.log({id: item.id, status: item.status, details: item.result.details});
}
globalThis.poolCursor = page.nextCursor;
console.log({pending: page.pendingItems, hasMore: page.hasMore, done: page.done});
```

Python uses keyword options and the same native cursor/page fields:

```python
pool_cursor = None  # Initialize once, after the first push.
```

```python
page = await pool.drain(cursor=pool_cursor, timeout=20, max_items=4)
for item in page['items']:
    print({'id': item['id'], 'status': item['status'], 'details': item['result']['details']})
pool_cursor = page['nextCursor']
print({'pending': page['pendingItems'], 'hasMore': page['hasMore'], 'done': page['done']})
```

The returned `pi.subagent.pool.drain.v1` page contains `poolId`, `jobId`,
`agent`, `status`, `sealed`, `admittedItems`, `settledItems`, `pendingItems`,
`items`, `cursor`, `nextCursor`, `hasMore`, `done` and `timedOut`. Each item has
`id`, terminal `status`, and its full native `result` (`content`, `details`,
optional `isError`). Item failures and cancellation are returned alongside
successful results; they do not throw away the page or its partial successes.

`cursor` echoes the acknowledged IDs supplied to this call. `nextCursor` adds
the returned IDs. Only a later call that supplies `nextCursor` acknowledges
those items for that reader. Reading does not delete retained results, change
other readers' cursors, or store an implicit guest cursor. After an interrupted
read or processing failure, reuse the previous cursor to reread available
results; newly settled items may also appear. Make any effects of processing a
result safe to repeat. Cursors use
`{schema: 'pi.subagent.pool.cursor.v1', poolId, itemIds: [...]}` with sorted,
unique one-based item IDs, and are bound to the owning native pool.

`hasMore` means additional unacknowledged settled results are ready beyond the
page limit. `done` becomes true only when the native pool is terminal and the
returned `nextCursor` covers every admitted item. An open pool with all current
items acknowledged remains open and reports `done: false`; call `finish()`
when no more work will be submitted. `drain()` also works after `close()` to
retrieve successes and terminal cancellation results.

The default timeout is zero: inspect immediately without waiting. A timeout
from 0 through 600 seconds waits natively for at least one unseen settled
item, terminal completion, or expiry. An empty unfinished page reports
`timedOut: true`; expiry does not cancel the pool. `maxItems` / `max_items`
accepts 1–4. An unused lazy pool has no native identity and rejects `drain()`;
the method never launches workers. A finished empty pool returns an empty
final page. Bridge, ownership, privacy and malformed protocol failures remain
errors, with no automatic retry or acknowledgment. The eval watchdog and
current host-call budget still apply.

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
size checks happen before dispatch. A nonempty push, `peek()`, `status()` or `drain()`
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
notice, after drain and child cleanup. OMP's keyed yield batches and custom
child-defined eval tools remain separate work. Native pools keep one prompt
per admitted item, explicit sealing, and fixed child lifetimes.

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
