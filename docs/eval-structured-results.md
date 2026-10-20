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
the kernel or forwarded to a nested tool. This adds sequential tool
orchestration, not OMP's retained workpool API or an eval background scheduler.

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

## Validation

`tests/eval_structured_reader.test.cjs` exercises the actual embedded JS factory
with a controlled host. It is diagnostic coverage, not a native integration
claim. `tests/eval_structured_reader_test.py` exercises the actual Python kernel
in subprocesses with a controlled protocol host, including queued/active
cancellation, reply draining and retained cells. It does not execute Rust
tools. `src/eval/bridge_result.rs` includes native reader, Python and QuickJS
regressions, plus serialization, malformed-input and whitelist tests. The
`eval_host_*` tests in `src/agent_tests.rs` exercise the actual Agent and kernels,
including native writes and reads, approvals, live plan changes, extension
hooks, disabled tools, recursive wrappers, call budgets and session changes.
The authoritative acceptance entry point remains
`dsr quality --tool pi_agent_rust`.
