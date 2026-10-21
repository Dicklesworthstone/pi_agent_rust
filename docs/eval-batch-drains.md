# Incremental judgment-batch results

`__pi_judge_batch_drain` reads newly settled items from a retained judgment
batch. It can wait for a result without waiting for the entire batch. Use it
from an Agent-owned Python or JavaScript Eval cell after `judgeBatch` /
`judge_batch` has returned a handle. No model binding is needed to read an
existing job, and a drain never starts, retries or cancels provider work.

The operation uses the current `jobs` tool policy and the captured conversation
identity. It does not make a standalone Eval instance able to inspect jobs.

## Read, process, then acknowledge

A cursor belongs to one job and one ordered key manifest. It records source
indices that the caller has acknowledged, not a server-owned consuming offset.
The same request is repeatable. Only replace your cursor after your application
has successfully processed the returned items:

```javascript
globalThis.reviewCursor = null;
// review is an existing retained judgeBatch handle.
globalThis.page = JSON.parse(await tool.call('__pi_judge_batch_drain', {
  jobId: review.id, cursor: reviewCursor, timeoutMs: 10000
}));
for (const item of page.items) {
  if (item.ok) display({key: item.key, answers: item.verdict.answers});
  else display({key: item.key, error: item.error});
}
reviewCursor = page.cursor;
display({done: page.done, jobFailed: page.jobFailed});
```

Python uses the identical protocol:

```python
import json
review_cursor = None
page = json.loads(await tool.call('__pi_judge_batch_drain', {
    'jobId': review.id, 'cursor': review_cursor, 'timeoutMs': 10000,
}))
for item in page['items']:
    display({'key': item['key'], 'result': item.get('verdict', item.get('error'))})
review_cursor = page['cursor']
display({'done': page['done'], 'jobFailed': page['jobFailed']})
```

Continue in later cells with the saved cursor. A reset loses kernel variables,
not admitted model jobs; an explicitly saved cursor and job ID can be used in
a fresh kernel. No credential or permission is embedded in a cursor. Omitting
it deliberately starts an independent reader and returns all currently retained
settled items. There is no claim of crash-durable cursor storage or exactly-once
application effects. A process restart or job eviction can make an ID unavailable.

A drain returns `pi.eval.batch-drain.v1` with:

- `items`: only newly acknowledged candidates, each with its original `index`,
  literal `key`, `ok`, and complete `verdict` or explicit `error`.
- `cursor`: the proposed next acknowledgment, bound to the job ID and a SHA-256
  digest of the ordered, typed keys. Treat it as opaque application state.
- `jobStatus`, `done`, and `jobFailed`: native lifecycle state, independently of
  whether this page has usable items. A failed batch can contain successful peers.
- `evidenceAvailable`, `total`, `settled`, and `complete`: whether retained
  evidence exists, its counts, and whether the batch itself fully finished.

A later input can finish before an earlier one. Each page is ordered by original
index, not chronological completion time; page sequences need not be globally
index-sorted. Numeric array indices and literal object keys (including Unicode)
retain their types. No queued/running item is turned into an answer or error.

`done: true` does not mean success. Cancellation and timeout can return the last
accepted partial evidence with `complete: false` and `jobFailed: true`. A generic
failure or privacy withdrawal can have no retained evidence at all; in that case
counts are null, items are empty, and the previous cursor is unchanged. Inspect
`review.output()` or the ordinary `jobs` tool for the complete native failure.
Old answers already delivered to the caller cannot be recalled by withdrawal.

## Waiting and authority

Arguments are exactly `jobId`, optional `cursor`, and optional `timeoutMs`.
The default is 30,000 ms; integers from zero through 600,000 are accepted.
`timeoutMs: 0` makes one nonblocking host lookup and can return an empty page
while the job is still running. A positive timeout bounds the entire drain,
including policy checks, readiness waiting and final result delivery. Expiry
raises `EVAL_BATCH_TIMEOUT` without returning or committing a new cursor; the
model job continues under its original lifetime. The Eval cell deadline can
stop a drain earlier and is never extended or paused.

Immediately available evidence uses one ordinary host call. A blocking drain
requires room for two of the existing 64 host calls and normally uses two:
one initial lookup, then a fresh policy-checked lookup after readiness changes.
Private native job observations are only wake-up hints (sampled at up to ten
observations per second); they never supply guest-visible result bodies. The
final lookup applies current permissions, hooks, session checks and output
projection. Revoked access or a changed session withholds the response.

The wait is invocation-owned and has no background reader thread or new
scheduler. Cancelling/dropping it cannot consume a result or cancel the model
job. Host refusal, malformed evidence, a changed key manifest, acknowledgment of
an unsettled index or a cursor for another job is an error, not an empty success.
All evidence is validated before forming a delta. Serialization borrows result
bodies and stops at 1 MiB; an oversized page is refused without a partial result
or a cursor update. The existing native job retention limits still apply.

This is an explicit-acknowledgment Rust-native API. It does not emulate OMP's
implicit consuming cursor, add retries/minimum-success policies, extend job
lifetimes, or authorize tools based on model verdicts.

## Validation

The new Rust regressions in `src/eval/judgment/drain/tests.rs` cover cursor
identity, keyed out-of-order deltas, replay, failures/interruption, malformed
observations, bounded output, actual jobs progress, current host projections,
revocation, host budgets, wait cancellation, and both native Eval kernel paths.
They are authored, not compiled or executed in the implementation environment.
`dsr quality --tool pi_agent_rust` could not start because `dsr` was unavailable
(exit 127). Static and fixture checks are not native runtime acceptance.
