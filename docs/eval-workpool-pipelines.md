# Streaming workpool map/reduce

`workpool.mapReduce()` in JavaScript and `workpool.map_reduce()` in Python
turn an iterable into native child-agent tasks, then reduce full tool results
as they settle. They reuse the existing session-owned pool and its current
policy checks. They do not create another scheduler, grant tools, relax
privacy, retry admission, or extend a child's lifetime.

The mapper returns a nonempty task string. The reducer receives the previous
accumulator and an outcome containing `index` (zero-based source position),
`input` (the original source value), and the native item's `id`, `status` and
full `result`. Images, typed data, failure details and other native result
content are not flattened into prose. Callbacks can be synchronous or async.

## JavaScript

```javascript
const report = await workpool.mapReduce(
    ['src/parser.rs', 'src/agent.rs', 'src/session.rs'],
    (path, index) => `Review ${path}. Return concrete findings and evidence.`,
    (findings, item) => [...findings, {
        path: item.input,
        sourceIndex: item.index,
        status: item.status,
        result: item.result,
    }],
    [],
    {
        pool: {agent: 'reviewer', concurrency: 3, timeoutSeconds: 120},
        timeout: 90,
        onPool: pool => { globalThis.reviewPool = pool; },
    },
);
display(report.value);
```

## Python

```python
report = await workpool.map_reduce(
    (path for path in ['src/parser.rs', 'src/agent.rs', 'src/session.rs']),
    lambda path, index: f'Review {path}. Return concrete findings and evidence.',
    lambda findings, item: [*findings, {
        'path': item['input'],
        'sourceIndex': item['index'],
        'status': item['status'],
        'result': item['result'],
    }],
    [],
    pool={'agent': 'reviewer', 'concurrency': 3, 'timeout_seconds': 120},
    timeout=90,
)
display(report['value'])
```

Use the same native workpool options already accepted by each language's
`workpool()` factory. The pipeline copies the Python options dictionary;
the existing factory validates and freezes native launch settings. Every
subsequent operation still goes through the current kernel bridge.

## Admission and ordering

Inputs may be synchronous or asynchronous iterables. The helper retains at
most `pool.concurrency` admitted-but-not-yet-reduced inputs (1–8), rather than
materializing the whole source or submitting every task at once. A completed
result frees a slot only after its reducer returns successfully.

The mapper runs in source order. The reducer runs sequentially in **result
arrival order**, not necessarily source order. Use the supplied `index` to
assemble a source-ordered result when order matters. At most four completed
items are read per native drain call; an early unfinished item does not hold
up later completed results.

`maxItems` / `max_items` defaults to 32 and accepts integers from 1 through 32,
matching the existing pool's lifetime capacity. A larger array/list is rejected
before pool creation. An unknown-length iterator uses one extra lookahead at
the limit to distinguish exhaustion from overflow. The extra value is not
mapped or admitted. Overflow is an explicit failure, not a successful truncated
pipeline. No fresh pool is silently created to evade native limits.

Empty input returns the initial value with `status: "empty"` and a lazy pool
handle whose native ID is still null. No child or aggregate native job is
started. Nonempty input is sealed with `finish()` once exhaustion is observed;
the helper drains through native terminal completion before returning.

## Results and error handling

Success returns `value`, `admitted`, `completed`, `succeeded`, `failed`, `pool`,
`cursor` and the native terminal `status`. `completed` counts **successful
reducer invocations**, not every native result that happened to finish.
`succeeded` and `failed` classify those reduced outcomes. The accumulator may
be any guest value, including false, zero or null/None. The helper does not
copy, constrain or reinterpret the accumulator.

By default a failed/cancelled item, or a result with `isError`, stops the
pipeline without passing that failure to a success-only reducer. Set
`raiseErrors: false` / `raise_errors=False` to reduce failed outcomes as data.
Counts and native status remain truthful; tolerating a failure does not turn
it into success. A failed native carrier also fails a default-policy pipeline,
even if all item results were delivered.

A `WorkpoolPipelineError` preserves the original exception (`cause` in
JavaScript, `__cause__` in Python) and exposes:

- `stage`, `item`, `progress`: where execution stopped and the last returned
  accumulator with completed-reduction counts.
- `pool`, `cursor`, `pending`: the existing pool handle, acknowledged result
  IDs, and bounded unprocessed item/input/index associations.
- `cleanup` and `cleanupErrors` / `cleanup_errors`: native cancellation receipt
  and secondary cleanup failures, without replacing the primary cause.

A result is acknowledged **only after the reducer returns**. If the second
item of a page fails in the reducer, the first item's acknowledgment is still
retained; the failing and unvisited items remain readable with the saved
cursor. The helper does not use the page's all-items `nextCursor` prematurely.
For example, a later cell can inspect `await savedPool.drain({cursor: savedCursor})`
or `await saved_pool.drain(cursor=saved_cursor)` after explicit recovery.
It does not automatically rerun failed reducers or resume the source iterator.
A reducer that performs a side effect and then raises remains responsible for
its own idempotency; a cursor cannot roll that side effect back.

On a caught failure the helper requests native `pool.close()` before closing
the source iterator. That cancels pending work rather than leaving a pipeline
running after an ordinary mapper/reducer/source error. The native close owns
cancellation and reaping. A lost launch acknowledgment is never retried;
`cleanup.unknownAdmission` can remain true, and an unknown native ID must be
resolved through ordinary job inspection rather than by resubmitting tasks.

Python preserves `asyncio.CancelledError`, interrupts and other control
exceptions instead of wrapping them as successful/ordinary task outcomes.
A new control exception during cleanup is likewise propagated, with the prior
failure retained as `pipeline_error` when needed.

## Timeouts and ownership

`timeout` defaults to 30 seconds and must be positive and at most 600. One
shared budget is passed to native result waits and checked between source,
mapper, reducer and control operations; it does not restart for each page.
A native empty timeout page raises instead of starting a guest polling loop.

This helper does not install another watchdog or detached Python task. An
arbitrary blocked guest callback/iterator is bounded by the **eval cell's
native watchdog**, not forcibly interrupted by the pipeline's wait budget.
Cleanup is best-effort under that same cell owner. Hard cell termination or
a closed bridge can prevent cleanup from running. Publish the handle early
with `onPool` / `on_pool` when later-cell inspection matters. Existing native
job ownership, deadlines and session cleanup continue to apply. A native
side effect already accepted by a child cannot be reversed by cancellation.

The pipeline owns its pool exclusively while running. Do not add unrelated
items through the published handle; foreign result IDs are rejected rather
than assigned to the wrong source input.

## Scope and validation

These helpers implement bounded streaming map/reduce over **one prompt per
native workpool item**. They do not implement OMP's keyed intermediate child
`yield`, export parent-kernel callback tools to children, or background an
entire eval cell.

Focused protocol regressions execute the actual JS/Python helpers with
controlled native-pool receipts:

```sh
node --test tests/eval_workpool_pipeline.test.cjs
python3 -m unittest discover -s tests -p 'eval_workpool_pipeline_test.py'
```

The Python bootstrap regression also checks installation before kernel main
and rejects workspace shadowing of the staged helper. Protocol fixtures are
not live-provider, native QuickJS, Rust or child-process acceptance. The
repository's authoritative quality entry point remains:

```sh
dsr quality --tool pi_agent_rust
```
