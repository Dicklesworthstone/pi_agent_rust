# Python tool-bridge ownership

The retained Python kernel allows synchronous `tool.read`, `tool.grep`,
`tool.find`, `tool.ls`, and `await tool.call(name, arguments)`. Agent-owned cells
call the enabled registry through the owning Agent's approval, plan, extension
and privacy policies. Standalone embeddings retain the local reader whitelist.
See [Agent-scoped orchestration](eval-structured-results.md#agent-scoped-tool-orchestration)
for tool availability, call budgets and native receipts.

## Live cell, not whichever cell runs next

A bridge call requires its originating cell's live context on that cell's
thread. Awaited tasks created within the current cell can use the bridge;
`asyncio.gather` still serializes complete request/reply exchanges while the
asyncio loop remains responsive during awaited replies. Saving a
helper such as `read = tool.read` is supported: calling it normally in a later
cell uses that later caller's context.

A retained task or saved `contextvars.Context` from a completed or failed cell
cannot borrow the next cell's authority. It receives `EVAL_BRIDGE_CLOSED`
before any host request. Native worker threads, including
`asyncio.to_thread(tool.read, ...)`, are refused rather than competing for the
protocol input stream. Use `await tool.call(...)` on the cell thread instead.
Reentrant exchanges receive `EVAL_BRIDGE_BUSY`, never a blocking lock wait.

These checks complement existing display/output ownership. They do not turn
arbitrary Python eval into a sandbox: code that deliberately accesses private
globals, descriptors, or Python's filesystem APIs retains its existing power.

## Request and reply integrity

Requests carry their cell ID and a monotonically increasing exchange ID.
Arguments must be JSON data. Requests are admitted before any host effect,
with a 1 MiB UTF-8 frame limit, bounded JSON shape, and a 128-character tool
name limit. Incremental encoding stops before building an oversized aggregate;
the admitted immutable snapshot is the one written to the host.

A reply must have the expected envelope, exact integer exchange ID, boolean
status, and textual result or error. Reads are bounded to 16 MiB characters
and also checked against 16 MiB UTF-8 bytes. The character bound can consume
more memory than the encoded-byte bound; it is not a 16 MiB heap guarantee.

A well-formed native tool refusal is an ordinary catchable `ToolBridgeError`.
Variables, later calls, and the kernel remain available. Invalid input that
never entered the protocol likewise does not poison the connection.

A missing, malformed, interrupted, or misaddressed reply poisons the exchange.
Further bridge calls are refused, and the final response reports `bridgeError`
even if cell code catches the exception. The native response validator rejects
that response with `EVAL_BRIDGE_PROTOCOL` and `EVAL_STATE_RESET`, retiring the
kernel before another cell. Accepted console/display frames remain subject to
the existing host's partial-output reporting; no output is replayed.

## Interruption boundary

User cancellation and hard host deadlines still discard the selected Python
kernel. POSIX Python may opt into an earlier `soft_timeout_secs` to retain state
after a clean computation interruption; see [soft deadlines](eval-soft-timeout.md).
Soft expiry during an active tool exchange still requires retirement, after
the owned reply drains within the unchanged hard deadline. An interrupted
exchange cannot be declared safe merely because Python caught an exception.

## Validation

`tests/eval_python_bridge.py` launches the actual bundled Python server and
records wire requests with a controlled protocol peer. These substrate tests
cover live calls, old tasks/contexts, copied and unattributed worker threads,
JSON admission, refusals, poisoned replies, and partial output. They do not
execute the Rust native tool implementations.

`tests/eval_bridge_ownership.rs` exercises public native EvalTool workflows.
The transport unit test verifies that every bridgeError marker retires the
connection, including an apparently successful final response. Run the
project's required `dsr quality --tool pi_agent_rust` for Rust acceptance.
