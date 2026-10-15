# Python eval tool ownership and asynchronous calls

Python eval retains variables and its asyncio loop, but a host-tool exchange
belongs to the cell actively executing it. The existing helpers remain:

```python
text = tool.read('src/main.rs')
matches = await tool.call('grep', {'pattern': 'TODO', 'path': 'src'})
```

Both routes use the same Rust-owned tool whitelist, permissions, conversation
lease and original timeout. A Python helper is not a separate I/O authority.

## Await tools without blocking the asyncio loop

`await tool.call(name, arguments)` lets timers, async tasks and subprocess
output pumps advance while the host tool replies. Multiple awaited calls are
serialized on the existing protocol, including under `asyncio.gather`; this
is not parallel host-tool execution. Local work can overlap that wait:

```python
import asyncio

async def prepare():
    await asyncio.sleep(0)
    return 'prepared'

first, second, local = await asyncio.gather(
    tool.call('read', {'path': 'one.txt'}),
    tool.call('read', {'path': 'two.txt'}),
    prepare(),
)
display({'local': local, 'first': first, 'second': second})
```

Arguments are JSON-encoded before queueing, so later mutation cannot retarget
a waiting call. At most 64 calls can be active/queued in one cell; each encoded
request is limited to 1 MiB. Invalid JSON, non-finite numbers, empty names and
oversized requests fail before dispatch. The normal tool whitelist still
permits only `read`, `grep`, `find` and `ls`; this change adds no tool authority.

Synchronous conveniences such as `tool.read` remain blocking. Using one while
an asynchronous exchange owns stdin returns `EVAL_BRIDGE_BUSY`, not a second
request or a blocking wait that could deadlock the event loop. Use another
awaited `tool.call` to queue behind it. Recursive argument-serialization calls
are likewise refused. Primitive-only JSON admission rejects custom argument
objects before invoking serialization hooks. Arbitrary synchronous guest work
and ordinary protocol output backpressure can still block Python; the Rust host
may deliver queued output updates after its pending tool completes.

## Cancellation and cell completion

Cancellation **before dispatch** removes a waiter without writing a request.
After dispatch, the bridge cannot revoke native work: it drains that one reply
before propagating cancellation. Repeated cancellation never releases stdin
early or replays the tool. Therefore a Python `wait_for` timeout is not a hard
upper bound on this drain; the original Rust-owned whole-cell deadline remains
in force and can retire the kernel/process tree. No timeout is paused or reset.

If the cell body ends with outstanding bridge tasks, admission closes first,
queued calls are cancelled, and any dispatched reply is drained. The cell
reports `EVAL_BRIDGE_PENDING` instead of claiming that fire-and-forget work
completed successfully. An ordinary, fully drained cancellation leaves the
kernel usable; completed native effects are not undone.

One dedicated, joined reader handles each awaited reply rather than occupying
the event loop or the guest's default executor. There is never more than one
active reader. A malformed, mismatched or incomplete reply produces
`EVAL_BRIDGE_PROTOCOL` and the existing native `bridgeError` retirement signal
instead of letting remaining bytes be interpreted as a new cell. Explicitly
reset Python before continuing after
that diagnostic. An unexpected event-loop stop also drains an outstanding
reader before retirement. Replies have both a 16 Mi-character read cap and a
16 MiB UTF-8 byte cap before parsing, with no acceptance of an unterminated EOF
frame. Normal host tool errors are distinct: they can be caught and do not
require a kernel reset.

## Retained tasks are not new-cell work

A pending asyncio task or saved Context object retains its creating cell's
identity. After that cell ends, a tool call from that context fails with
`EVAL_BRIDGE_CLOSED` before writing a request. A later cell does not transfer
ownership to it. Await tool work before ending its owning cell.

Ordinary functions and saved references to `tool.read` remain reusable. Calling
one normally in a new cell uses that new invocation's context. The restriction
is on executing a closed context, not on where a function was defined.

Worker threads, including `asyncio.to_thread` with a copied live context,
cannot initiate bridge calls. They fail with `EVAL_BRIDGE_THREAD` (or
`EVAL_BRIDGE_CLOSED` without a live context). The bridge's internal reader does
not execute tools or copy cell ContextVars. Existing context-aware printing
for awaited guest worker threads remains unchanged.

These rules prevent accidental cross-cell dispatch and protocol races;
arbitrary Python eval is not a sandbox against deliberately modified
interpreter internals. Host effects are never rolled back or automatically
replayed.

## Validation

An independent suite launches the exact Python kernel subprocess and checks
its real JSONL protocol. All 21 tests passed, including the nine preceding
ownership cases, queued cancellation, repeated post-dispatch cancellation,
end-of-cell draining, reply fragmentation, limits, recovery and retirement.
The old-task regression fails against the original source by observing an
unwanted read. Three initial boundary/capacity tests returned the host reply too
early; they were corrected to synchronize on explicit kernel events, with their
initial failures retained in session evidence. The Python suite remains in
that evidence because its upload was blocked, not because it passed DSR.
The concurrently added repository suite at `tests/eval_python_bridge.py`
is preserved unchanged: all 17 of its subprocess cases also passed against
the merged implementation. Its stricter primitive-only argument policy,
exact reply envelopes, per-cell request IDs and `bridgeError` host signal
remain intact. The local re-entry regression now checks that custom objects
are rejected before serialization, rather than admitted with a callback guard.

Four tests in `tests/eval_python_bridge.rs` exercise public EvalTool ownership,
awaited native reads, structured displays and unchanged host denial. They are
authored but unexecuted: `dsr` is unavailable in the implementation environment.
Rust compilation, formatting, Clippy, full-host integration and cross-platform
acceptance remain unverified. The authoritative entrypoint remains:

```sh
dsr quality --tool pi_agent_rust
```
