# Structured results inside Python and JavaScript eval

`await tool.result(name, args)` executes one call through eval's existing
read-only native bridge and returns a JSON object rather than flattened text:

```javascript
const result = await tool.result('read', {path: 'plot.png'});
const image = result.content.find(block => block.type === 'image');
if (image) display(image.data, image.mimeType);
```

The envelope has `schema: "pi.eval.tool_result.v1"`, native `content` blocks,
`details` (including null, arrays and scalar values), and boolean `isError`.
A returned native tool error is data with `isError: true`; policy, input,
transport and serialization failures reject the promise. There is no retry.
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

This enables `read`, `grep`, `find` and `ls` only. It does not enable live Agent
registry re-entry, subagents, MCP tools, judge calls or retained workpools.
Both helpers are available in session and per-call kernel modes. The original
Python `tool.call` and synchronous reader helpers continue to return text.

`__pi_tool_result` is the internal protocol operation. Its object requires
exactly `name` and object-valued `args`; the inner name passes the ordinary
whitelist, with no recursive unwrapping or permission fallback.

## Validation

`tests/eval_structured_reader.test.cjs` exercises the actual embedded JS factory
with a controlled host. It is diagnostic coverage, not a native integration
claim. `tests/eval_structured_reader_test.py` exercises the actual Python kernel
in subprocesses with a controlled protocol host, including queued/active
cancellation, reply draining and retained cells. It does not execute Rust
tools. `src/eval/bridge_result.rs` includes native reader, Python and QuickJS
regressions, plus serialization, malformed-input and whitelist tests. The
authoritative acceptance entry point remains `dsr quality --tool pi_agent_rust`.
