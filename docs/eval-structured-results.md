# Structured results inside JavaScript eval

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

The complete encoded envelope is limited to 4 MiB during serialization;
details are also limited to 64 levels and 131,072 JSON values. An oversized
result fails explicitly instead of returning an apparently successful fragment.
The existing `tool.call` returns text and rejects native errors as before, with
an explicit 1 MiB text limit. Cells may retain returned objects in their normal
namespace. Requests still use the original session owner and cell deadline.

This enables `read`, `grep`, `find` and `ls` only. It does not enable live Agent
registry re-entry, subagents, MCP tools, judge calls or retained workpools.
The JavaScript helper is available in both session and per-call kernel modes.
Python's existing helpers remain text-only in this change.

`__pi_tool_result` is the internal protocol operation. Its object requires
exactly `name` and object-valued `args`; the inner name passes the ordinary
whitelist, with no recursive unwrapping or permission fallback.

## Validation

`tests/eval_structured_reader.test.cjs` exercises the actual embedded JS factory
with a controlled host. It is diagnostic coverage, not a native integration
claim. `src/eval/bridge_result.rs` includes native reader and QuickJS regressions,
plus serialization, malformed-input and whitelist tests. The authoritative
acceptance entry point remains `dsr quality --tool pi_agent_rust`.
