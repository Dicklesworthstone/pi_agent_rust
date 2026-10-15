# Python eval tool ownership

Python eval retains variables and its asyncio loop, but a host-tool exchange
belongs to the cell actively executing it. The existing helpers remain:

```python
text = tool.read('src/main.rs')
matches = await tool.call('grep', {'pattern': 'TODO', 'path': 'src'})
```

Both routes use the same Rust-owned tool whitelist, permissions, conversation
lease and original timeout. A Python helper is not a separate I/O authority.

## Retained tasks are not new-cell work

A pending asyncio task or saved Context object retains its creating cell's
identity. After that cell ends, a tool call from that context fails with
`EVAL_BRIDGE_CLOSED` before writing a request. Switching to a later cell does
not transfer ownership to it. Await delegated work before ending its owning
cell; do not rely on a retained background task to initiate later tool calls.

Ordinary functions and saved references to `tool.read` remain reusable. Calling
one normally in a new cell uses that new invocation's context. The restriction
is on executing a closed context, not on where a function was defined.

Worker threads, including `asyncio.to_thread` with a copied live context,
cannot read or write the private bridge protocol. They fail with
`EVAL_BRIDGE_THREAD` (or `EVAL_BRIDGE_CLOSED` without a live context). Call tools
on the cell's event-loop thread. The existing context-aware printing support
for awaited worker threads is unchanged.

Only one synchronous bridge exchange can own stdin. Recursive calls during
argument serialization fail with `EVAL_BRIDGE_BUSY` before a second request
is emitted. A normal host tool error is not retried and can be caught; it does
not prevent a subsequent foreground call. Awaited calls still use the same
serialized synchronous exchange at this checkpoint.

This prevents accidental cross-cell dispatch and protocol races; arbitrary
Python eval is not a sandbox against code deliberately modifying interpreter
internals. Completed host effects are never rolled back or automatically
replayed.

## Validation

An independent Python suite launches the exact kernel source as a subprocess
and checks its real JSONL protocol. Its ownership regression reproduces an
unwanted old-task read on the previous source and passes after the fix. Nine
Python tests passed in the implementation environment; that suite remains
in the session evidence because its upload was blocked. Two regressions in
`tests/eval_python_bridge.rs` exercise the public EvalTool path. Those Rust
tests were authored but not executed because `dsr` was unavailable.
The repository's quality authority remains `dsr quality --tool pi_agent_rust`;
standalone Python checks are not full DSR or cross-platform acceptance.
