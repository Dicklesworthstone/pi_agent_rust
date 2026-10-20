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

`workpool()` distributes a bounded batch of prompts among reusable native
workers. Tasks assigned to the same worker share its conversation, making a
pool useful for related inspections or staged implementation work. Every
worker is a real child process with its own native queue.

```javascript
globalThis.pool = await workpool({agent: 'task', concurrency: 3});
globalThis.items = await pool.push([
    'Inspect the parser for correctness issues.',
    'Inspect session persistence for data-loss risks.',
    'Inspect cancellation and child-process cleanup.',
    'Check the error handling around your assigned subsystem.',
]);
```

The acknowledged tasks keep running after the cell returns. In a later cell:

```javascript
const results = await wait(pool.items, {timeout: 20, raiseErrors: false});
console.log(await pool.status());
await pool.close();
```

Python exposes the same operations:

```python
pool = await workpool(agent='task', concurrency=3)
items = await pool.push([
    'Inspect the parser for correctness issues.',
    'Inspect cancellation and child-process cleanup.',
])
results = await wait(pool.items, timeout=20, raise_errors=False)
snapshot = await pool.peek()
cleanup = await pool.close()
```

Creation is lazy: `workpool()` makes no native calls until the first `push()`.
Concurrency defaults to four and accepts 1–8 workers, sharing the same global
eight-job limit as other native jobs. A pool admits at most 32 items over its
entire lifetime. All prompts in a push are validated before any admission;
the worker prompt limits above apply. JavaScript requires an array of strings;
Python accepts a list or tuple. Empty batches are allowed.

Initial tasks start workers up to the configured concurrency. Later pushes
poll existing workers once, prefer an idle worker with the fewest admitted
items, and otherwise balance queued turns across live workers. Each push
returns fixed turn handles in input order. `pool.items` retains all acknowledged
handles across pushes as a frozen array or Python tuple. Failed or expired
workers are never silently replaced, and admission is never automatically retried.

| Operation | Result |
| --- | --- |
| `push(tasks)` | Acknowledged turn handles for this batch, in input order. |
| `peek()` | Each retained item's worker/turn identity, status and native result; unavailable reads include an error. |
| `status()` | Worker and item counts, including queued, running, completed, failed, cancelled and unavailable items. |
| `close()` | Close admission and cancel all known workers, including their running and queued turns; report cleanup per worker. |

Pool commands are serialized and **must be awaited**. Native queues perform
the execution between cells; no guest timers or callbacks are retained to
dispatch subsequent work. Polling `peek()` or `status()` makes one host call
per retained item, so it counts toward the current cell's 64-call allowance.
The normal cell watchdog bounds each command and wait.

If admission fails partway through a push, the pool closes further admission
and preserves every acknowledged handle in `pool.items`. The raised error's
`admitted` field contains this push's acknowledged handles, and `requested`
records the batch size. Already admitted tasks continue until they finish or
are cancelled. Inspect them and close the pool instead of retrying the same
batch, which could duplicate effects. Cancellation or an interrupted cell can
also leave partial admission; acknowledged handles remain inspectable while
the kernel survives.

A lost new-worker acknowledgment can leave an admitted carrier whose identity
the guest never received. Such a pool reports `unknownAdmission: true`.
`close()` distinguishes `allKnownClosed` from `allClosed`; the latter stays
false while an unknown admission is possible. Use native `jobs list` and
`jobs cancel` to inspect and retire any remaining owned carrier. Repeated
`close()` retries only workers whose termination has not been acknowledged.

Pool options are `name`, `agent`, `concurrency`, `cwd`, `scope`,
`isolation: "none"`, and `timeoutSeconds` / `timeout_seconds`. Names are labels
for the guest object, not session-wide registrations. Worker deadlines include
idle and queued time and are not renewed per item. Pools stay open after a
batch drains until explicitly closed or their workers expire.

This bounded native pool provides shared conversation, queued follow-ups and
explicit result collection. OMP's context-load scheduling, keyed yield batches,
aggregate pool completion delivery and custom child-defined eval tools remain
separate work. Pool `close()` cancels running work as well as queued work.

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
mixed waits, real protocol field names, pool reuse, partial admission,
interrupted-cell recovery and ambiguous cleanup reporting. Native worker
regressions in `src/subagents/execution/worker.rs`, `src/subagents/worker.rs`
and `src/jobs/host_task.rs` cover same-process conversation reuse, compaction
barriers, cancellation/reaping, privacy projection and session ownership. The
`eval_host_*` tests in `src/agent_tests.rs` exercise the actual Agent and kernels,
including native writes and reads, approvals, live plan changes, extension
hooks, disabled tools, recursive wrappers, call budgets and session changes.
The authoritative acceptance entry point remains
`dsr quality --tool pi_agent_rust`.
