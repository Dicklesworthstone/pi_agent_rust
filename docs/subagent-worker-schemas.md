# Structured results from reusable workers

Reusable RPC workers can now enforce a frozen JSON Schema on every completed
turn. The child retains its conversation and process; the host retains a typed
`data` value only when that turn's complete terminal answer validates. This
closes the previous rejection of schema-bearing definitions on `worker_start`.
It does not change independent-task schema retry behavior or add schema support
to reusable pools.

## Launch with an explicit contract

Call the native `subagent` tool:

```json
{
  "operation": "worker_start",
  "agent": "reviewer",
  "task": "Inspect the parser and return concrete findings.",
  "outputSchema": {
    "type": "object",
    "required": ["findings"],
    "additionalProperties": false,
    "properties": {
      "findings": {"type": "array", "items": {"type": "string"}}
    }
  },
  "schemaMode": "strict",
  "timeoutSeconds": 120
}
```

The selected agent must already exist. `outputSchema` overrides its definition's
`output_schema` for this worker only. `schemaMode` may be omitted (validation is
always strict), but an explicit value must be `strict` and requires a schema.
A send cannot change the contract. Explicit null is an invalid schema, not a way
to silently disable validation; boolean JSON Schemas are accepted.

Use the returned `workerId` with `worker_wait`, `worker_status`, `worker_send`,
and `worker_close`, as before. Successful results include:

```json
{
  "status": "completed",
  "schemaValid": true,
  "schemaRetries": 0,
  "data": {"findings": ["Missing input validation in the parser."]}
}
```

Those fields are inside `details.turn.result.details.results[0]` in a direct
worker observation. The native turn result retains its original output, PID,
and ordinary result metadata. A validated JSON null is present as `data: null`;
absence of `data` is not a validated null.

## Use the existing Eval handles

A named agent may instead declare the schema in its Markdown frontmatter:

```text
---
name: typed-reviewer
description: Review code and return structured findings
tools: [read, grep, find, ls]
output_schema: {"type":"object","required":["findings"],"properties":{"findings":{"type":"array","items":{"type":"string"}}}}
---
Inspect the assigned code. Report only concrete findings supported by evidence.
```

Then the existing high-level factories work without new options:

```javascript
globalThis.worker = await agent('Inspect the parser.', {
  agent: 'typed-reviewer', keepAlive: true, timeoutSeconds: 120
});
const output = await worker.wait(20);
display(output.details.results[0].data);
```

```python
worker = await agent('Inspect the parser.', agent='typed-reviewer',
                     keep_alive=True, timeout_seconds=120)
output = await worker.wait(20)
display(output['details']['results'][0]['data'])
```

The high-level factories still reject inline schema options for warm workers;
use the named definition or the native launch above for an explicit override.
After a native launch, `agent.attachWorker(workerId)` in JavaScript or
`agent.attach_worker(worker_id)` in Python restores the ordinary result handle.
Enable new sends explicitly when recovering, as documented in
[worker recovery](eval-agent-recovery.md).

## Acceptance and failure semantics

The complete schema is checked under admission-time privacy policy before any
carrier or child is published. The actor prepares its validator from the same
owned definition before process launch and appends the contract to each turn's
prompt, including after compaction. Changing the definition file does not change
an already-running worker's contract, model, tools, workspace, or lifetime.

Validation requires exactly one JSON value. Prose wrappers, Markdown fences,
trailing values, duplicate decoded keys, and schema mismatches fail the turn.
No extraction heuristics, coercion, automatic repair prompt, or retry is used.
A failed turn exposes `schemaValid: false`, `schemaRetries: 0`, a static error,
and no `data`; its native result is unsuccessful. The process can accept later
explicit turns, and already queued independent turns continue. Do not treat
queue ordering as an acceptance dependency; inspect each result before sending
work that relies on it. Existing high-level waits raise for an unsuccessful
turn, while `output()` preserves its full failure evidence.

Validation only happens after a fresh protocol-validated terminal response and
the post-turn persistence/compaction barrier. An acknowledgment, streamed prefix,
cancelled request, or earlier turn cannot become a typed result. Schema failure
does not roll back filesystem or tool effects performed by the child. A matching
schema establishes structure, not factual correctness or approval to act.

Schemas are limited to 16 KiB, bounded depth and nodes; answers to 32 KiB.
Local definitions/references are supported. External schema retrieval is
explicitly refused. Existing command, result-retention, turn, job-capacity,
permission, cancellation and deadline limits remain authoritative; no budget is
renewed. Schema compilation/validation is synchronous bounded-input work, not a
hard CPU-time guarantee for arbitrary schemas.

If privacy projection changes any part of a schema-valid native result, that
result is withheld rather than retaining a validation claim for different data.
This applies at storage and on later worker reads under stricter policy. Plain
text turns retain their existing redaction behavior. Known results do not gain
new authority through recovery, and a host restart does not resume the worker.

## Validation status

Five contract regressions and six native RPC-worker regressions cover strict
JSON/schema behavior, null, resource and shape bounds, frozen definitions,
process reuse, explicit overrides, failure without replay, cancellation, and
privacy at storage/read boundaries. They are authored, not compiled or executed
on the implementation host. Ten standalone checks exercise the exact shell
peer's syntax and protocol encodings; they do not execute the Rust actor.
The required `dsr quality --tool pi_agent_rust` could not start because `dsr`
was unavailable (exit 127). Native compilation, tests, formatting, Clippy and
cross-platform acceptance remain unverified.
