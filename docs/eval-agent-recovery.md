# Recover native workers and pools after an Eval reset

JavaScript and Python Eval can reconstruct reusable-worker and workpool handles from their
native IDs. Attachment reads current state through the existing `subagent` host
bridge. It does not start a child, resubmit a task, recreate a conversation,
renew a deadline, or retain an earlier approval. The existing native registry
and session ownership remain authoritative.

## Recover a worker and select an existing turn

Keep `worker.id` outside the kernel before resetting it. After reset:

```javascript
globalThis.worker = await agent.attachWorker(savedWorkerId);
const latest = await worker.output();
display(latest);
const earlier = await worker.turn(1);
display(await earlier.output());
```

`attachWorker` selects the latest native turn by default. Pass `{turnId: 2}` to
select a particular existing turn. `worker.turn(2)` performs a current-policy
lookup and returns a fixed turn handle without changing the worker's selected
turn. An absent turn fails rather than choosing a different one. Recovered
workers and turns work with the existing global `wait([...], {timeout})`.

Recovered workers initially have `sendAllowed: false`. This disables new
submissions on that guest handle, not the existing native queue. Status, output,
wait, turn selection and explicit close/cancel remain available. Closing a turn
still closes its whole worker, including every queued turn.

After inspecting what the native worker already accepted, explicitly permit a
new, distinct prompt:

```javascript
globalThis.worker = await agent.attachWorker(savedWorkerId, {allowSend: true});
globalThis.followup = await worker.send('Review the accepted change for missing tests.');
```

The returned handle uses the original frozen agent definition, model, tools,
working directory and lifetime. Attachment cannot override any launch setting.
`worker.recovered` distinguishes a reconstructed handle. A closed or expired
native worker remains closed: permitting guest sends does not reopen it.

## Ambiguous send acknowledgments

Newly launched and recovered workers now stop accepting further guest sends
when a send acknowledgment is lost, rejected or malformed. The original error
still raises; `unknownAdmission` becomes true and `sendAllowed` becomes false.
Known turn handles remain readable. No prompt is automatically retried, and a
concurrent successful acknowledgment cannot erase the uncertainty.

Finishing a cell while a send is pending also closes further sends when that
handle is next used. Await every send. An old callback cannot be reused as an
acknowledgment in a later cell. Native receipts remain the source of admission
and failure evidence; catching a JavaScript exception does not remove them.

The guest cannot distinguish every denial before dispatch from a lost reply
after dispatch, so it intentionally treats either as uncertain. Inspect the
worker using its ID, compare the native turns with the requested work, then
explicitly reattach with `allowSend: true` only for genuinely new submissions.
A fresh handle does not reopen the old uncertain handle. This is not automatic
resume or a transactional exactly-once guarantee.

## Recover a pool and its entire item manifest

Preserve `pool.poolId` and `pool.jobId`, or obtain them from the owning session's
`jobs list`: pool carrier labels include `reusable subagent pool <poolId>`.
A worker ID is not a carrier job ID; attachment does not guess or translate IDs.

```javascript
globalThis.pool = await workpool.attach(savedPoolId);
display((await pool.peek()).items);
const finished = await wait(pool.items, {timeout: 20, raiseErrors: false});
display(finished);
```

The complete bounded native manifest reconstructs fixed `pool.items` handles
in admission order, including items whose original admission reply was lost.
Full results still come from each item's existing `output()` or `wait()`.
Read results before assuming a summary is the full answer. Attachment does not
consume results or completion notices.

Recovered pools also retain the ordinary `drain()` method and context-load
observations. In JavaScript, `await pool.drain({cursor: savedCursor, timeout: 0})`
and in Python, `await pool.drain(cursor=saved_cursor, timeout=0)` collect finished
full results without admitting more work. Save `nextCursor` only after processing
the page. Repeating the prior cursor rereads retained results, not model work.
The guest's default closed-admission flag does not prevent result collection.

Recovered pools initially disable new pushes (`closed: true` refers to this
guest admission state). Existing native work continues. Explicit `finish()`
seals the native queue and returns its aggregate job; `close()` cancels the
whole pool. An already-sealed snapshot supplies the aggregate handle without
resending `pool_finish`. Failed/cancelled aggregates keep their ordinary error
semantics. Only a currently open pool can accept additional work:

```javascript
globalThis.pool = await workpool.attach(savedPoolId, {allowPush: true});
const extra = await pool.push(['Review a newly discovered independent component.']);
const aggregate = await pool.finish();
```

Only the existing pool ID and the explicitly supplied new tasks reach
`pool_push`. Agent selection, concurrency, working directory and the original
lifetime stay native and frozen. Recovery does not restore the old guest-only
name; the handle uses `recovered-workpool` and `recovered: true`.

A lost push acknowledgment follows the same existing no-replay policy:
`peek()` can reconcile admitted items, but does not reopen that guest handle.
Explicit recovery is an opportunity to inspect accepted work, not proof that
resending it is safe. Separate recovered handles do not acquire an exclusive
lease; use one coordinator when admitting additional tasks.

## Authority, validation and limits

Every attachment, read and control re-enters current host policy. Foreign,
missing, evicted or wrong-kind identities fail; there is no fallback launch.
Standalone Eval does not gain `subagent` authority. IDs and typed recovery
options are validated before dispatch; option changes while awaiting a snapshot
cannot enable sends or pushes. Inherited permission flags do not enable them.
Existing per-cell tool-call/deadline limits and native task/retention bounds
remain unchanged. Kernel reset is recoverable; a host-process restart is not.
A failed host read never falls back to a cached result.

## Python recovery

Python uses keyword options and snake_case compound names:

```python
worker = await agent.attach_worker(saved_worker_id)
display(await worker.output())
earlier = await worker.turn(1)
display(await earlier.output())
pool = await workpool.attach(saved_pool_id)
results = await wait(pool.items, timeout=20, raise_errors=False)
display(results)
```

`agent.attach_worker(id, turn_id=2)` selects an existing turn. Explicit
`allow_send=True` or `workpool.attach(id, allow_push=True)` permits new,
distinct submissions only after inspecting native state. The default never
submits additional work. `worker.send_allowed`, `worker.unknown_admission`,
`worker.recovered` and `pool.recovered` expose the corresponding guest state.

Freshly launched Python workers use the same acknowledgment safeguard. A
cancelled send, host error or invalid reply disables subsequent sends and
preserves the original exception. The Python bridge remains responsible for
draining an already-dispatched exchange; the helper neither abandons the reply
nor resubmits it. Invalid local arguments fail before changing admission state.
Successful concurrent acknowledgments cannot clear another send's uncertainty.

The host stages a compiled-in recovery module beside the kernel and browser
bootstrap. Before the receive loop, that module extends the existing concrete
worker/pool classes and installs the recovery factories. Existing factories
and mixed waits resolve those classes through the kernel's globals, so no
second transport or worker implementation is introduced. The private staging
directory is not added to `sys.path`; workspace imports and the original user
argument shape are preserved. This is not a security sandbox against Python
code deliberately changing interpreter internals.

## Validation

The complete production JavaScript factory is exercised by
`node --test tests/eval_agent_recovery.test.cjs`: 27 controlled-transport cases
cover recovery, turn selection, partial/failed results, shared waits, malformed
responses, current policy, pending cells, concurrent sends, no replay and sealed
pools, including recovery followed by incremental collection and context-load
inspection. Python's production recovery module has 18 controlled-kernel adapter
tests in `tests/eval_agent_recovery_test.py`; those inherited kernel handles are
explicit test doubles, not real native workers. The updated bootstrap's 25
controlled checks also pass. Seven Rust tests in `tests/eval_agent_recovery.rs`
target the actual QuickJS/Python Eval bridge, reset, native call receipts and
standalone refusal, including inherited native pool drains in both languages.
They are authored, not compiled or executed on the
implementation host. Python integration cases require Python >= 3.10.

`dsr quality --tool pi_agent_rust` remains the authoritative quality entry point.
DSR was unavailable here (exit 127). Controlled guest tests do not establish
native Rust, child-process, live-model or cross-platform acceptance.
