# Dependency-ordered native subagent workflows

`subagent` accepts a `workflow` as an alternative to a single delegation,
independent `tasks`, or a sequential `chain`. Use it for fork/join work: two
implementations can run together, followed by a review that starts only after
both results and their workspace changes have been accepted.

```json
{
  "workflow": [
    {
      "id": "review",
      "needs": ["api", "ui"],
      "call": {"agent": "reviewer", "task": "Review the accepted API and UI changes."}
    },
    {
      "id": "api",
      "call": {
        "agent": "task", "task": "Implement the assigned API change.",
        "isolation": "worktree", "isoApply": "apply"
      }
    },
    {
      "id": "ui",
      "call": {
        "agent": "task", "task": "Implement the assigned UI change.",
        "isolation": "worktree", "isoApply": "apply"
      }
    }
  ],
  "concurrency": 2,
  "timeoutSeconds": 300
}
```

Node order is the result order, not necessarily the execution order. Forward
references are supported. A node ID is 1–64 ASCII letters, digits, underscores
or hyphens. `needs` lists direct predecessors; omitted or empty means a root.
Cycles, unknown IDs, duplicate IDs/edges and self-dependencies fail before any
child launch. There are at most 32 nodes and 256 KiB of encoded request data;
each task and agent string is limited to 64 KiB. Concurrency is 1–8, default 4.

## One native execution path

The graph is divided into deterministic topological waves. Ready nodes in each
wave remain in authored order and run through the existing parallel child
runner. That runner captures one baseline per source root before starting the
wave, including queued siblings and their corrective retries. It settles all
children and accepts isolated patches in authored order. Only then does the
next wave capture its baseline and begin. Consequently, a dependent worktree
sees accepted predecessor edits, not a provisional sibling checkout.

This is a deliberate barrier, not an ASAP scheduler: an unrelated slow node in
a wave can delay the next wave. Independent nodes beyond a failed branch still
run when their own dependencies succeed. Use `needs` for actual data/workspace
dependencies; ordinary concurrent non-isolated children still share files and
require coordination. `isoApply: keep` or `drop` does not transfer those edits
into a dependent workspace. The graph never invents a merge or changes a
node's native disposition.

Definitions, tool restrictions, model selection, child protocol, process-tree
cleanup, hub registration, schema correction and workspace acceptance retain
the existing implementation. The parent model is captured once. One original
host-bounded deadline covers discovery, all waves, queueing and retries; a
later wave receives no fresh allowance. Arbitrary child activity and synchronous
filesystem/Git operations have the same cooperative boundaries as ordinary
delegation. This does not add recursive delegation or a new guest interpreter.

Each node's `call` contains ordinary per-task fields: `agent`, `task`, `cwd`,
`isolation`, `isoApply`, `outputSchema` and `schemaMode`. All call shapes and
workspace options are validated up front. Agent discovery and all effective
schema compilation also finish before the first child. Native filesystem
conditions are still checked at execution; this is not a transactional dry run.
Mixed modes and unknown fields are refused rather than silently ignored.

## Typed handoffs between predecessors and dependants

Add `inputs` beside a node's `call` to pass selected schema-validated results
into that child's task. Each named input has a `from` node ID and a JSON
`pointer` into that node's accepted `data`, not into its entire native result:

```json
{
  "workflow": [
    {
      "id": "inspect",
      "call": {
        "agent": "scout", "task": "Identify the files relevant to the assigned change.",
        "outputSchema": {
          "type": "object", "required": ["files"],
          "properties": {"files": {"type": "array", "items": {"type": "string"}}}
        }
      }
    },
    {
      "id": "implement", "needs": ["inspect"],
      "inputs": {"files": {"from": "inspect", "pointer": "/files"}},
      "call": {
        "agent": "task", "task": "Implement the assigned change using the supplied file findings.",
        "isolation": "worktree", "isoApply": "apply"
      }
    },
    {
      "id": "review", "needs": ["implement"],
      "call": {"agent": "reviewer", "task": "Review the accepted implementation."}
    }
  ]
}
```

A join may select inputs from multiple predecessors. Every source must also
appear directly in `needs`, even when it is already a transitive ancestor.
Forward references work. A source must declare `outputSchema` on its call or
inherit a schema from its agent definition; this is checked before any child
launch. A boolean JSON Schema is valid too. Schema-free prose is never guessed
into structured data. Existing native schema validation and corrective retries
remain authoritative.

Pointers preserve literal keys and JSON types: `/a~1b/~0key/0` selects element
zero beneath keys `a/b` and `~key`. Empty pointer selects complete `data`,
including a scalar or null. Missing data is not null and has no fallback. There
is no dotted-path shorthand, template language, script execution or coercion.
Input names use the node-ID character rule, with at most 32 inputs per node;
pointers are at most 1,024 UTF-8 bytes and 32 segments. Unknown fields, null
input maps, invalid escapes and indirect/unknown source references are errors.

The native child receives its literal authored task followed by a
`<workflow-inputs>` JSON data block. Values are serialized, not interpolated
as source, CLI arguments or task templates. `<` is escaped while serializing
so data cannot forge the closing delimiter. Existing `{previous}` or other
placeholder-like text stays literal in workflow tasks and input values.
The complete expanded task, including framing and escaped JSON, is bounded
to 64 KiB; counting occurs before copying data, and the serialization sink
enforces the expansion bound. Results are borrowed from this invocation's
accepted records rather than loaded from a process-global cache.

Input binding never changes the selected agent, model, tools, cwd, isolation,
output schema or deadline. This is data provenance and framing, **not a
prompt-injection sandbox**: values still become model context, and a child can
act on that context within its ordinary host policy. Native progress, hub and
revival records may contain the actual expanded prompt. The terminal workflow
node's `result.task` remains the authored assignment to avoid repeating the
input payload there; ordinary result output/data disclosure is unchanged.

If data is unavailable, missing at the pointer or too large, that node is
skipped **before** its native invocation with `dependency_data_unavailable`,
`dependency_input_missing` or `dependency_input_limit`. `blockedBy` lists its
selected input sources. Descendants skip normally; independent branches can
continue. The source result remains available for inspection, and the failed
handoff is not silently retried or replaced with raw child output.

## Typed decision gates

A schema-valid reviewer result can still say not to proceed. Add `when` beside
`call` to make the next native task depend on that actual decision:

```json
{
  "workflow": [
    {
      "id": "review",
      "call": {
        "agent": "reviewer", "task": "Choose approve or repair for the proposed implementation.",
        "outputSchema": {
          "type": "object", "required": ["verdict"],
          "properties": {"verdict": {"enum": ["approve", "repair"]}}
        }
      }
    },
    {
      "id": "repair", "needs": ["review"],
      "when": {"from": "review", "pointer": "/verdict", "equals": "repair"},
      "call": {
        "agent": "task", "task": "Repair the implementation identified in the review.",
        "isolation": "worktree", "isoApply": "apply"
      }
    },
    {
      "id": "report", "needs": ["review"],
      "when": {"from": "review", "pointer": "/verdict", "equals": "approve"},
      "call": {"agent": "scout", "task": "Summarize the accepted implementation."}
    }
  ]
}
```

`when` requires exactly `from`, `pointer` and a literal `equals` value. Its source
must be a direct `needs` predecessor with an effective output schema; all agents,
schemas and gate shapes are validated before any child launch, including branches
that will not be chosen. Comparisons use the predecessor's accepted, validated
`data`, never prose or a provisional stream. JSON values are not coerced: false,
zero and the string `"0"` differ. Empty pointer selects all data; escaped keys and
null use the same pointer rules as inputs. A missing value is an error, **not** a
false condition. Conditions are at most 16 KiB encoded, pointers at most 1,024
bytes/32 segments. There are no expressions, regexes, negation or implicit defaults.

A non-match marks the branch `not_selected`; descendants requiring it are also
not selected. No child, hub entry, input expansion or worktree capture occurs for
these nodes. Intentional nonselection alone does not fail the workflow. Rejected,
cancelled, schema-invalid or unavailable dependencies still block as errors, even
when another prerequisite was intentionally not selected. A downstream task that
needs **both** alternative branches will therefore not run; dependencies retain
their all-required semantics.

The node's `condition` field and text show `matched`, `not_matched`,
`condition_value_missing` or `condition_data_unavailable` when evaluated. Expected
and actual values are not repeated in decision diagnostics; ordinary source
results may still expose their data. Nonexecuted nodes retain null result/hub/wave
fields. `notSelectedNodes` counts intentional exclusions separately from errors in
`skippedNodes`. If no condition matches, no branch runs: selection is not an
exhaustiveness guarantee. Use a constrained producer schema for known alternatives.

This is agent-result routing, **not human approval or new tool authority**. The
selected child still needs its ordinary host permissions. A gate does not undo
earlier changes; put review before the mutation it is intended to control. All
evaluation shares the original workflow deadline and acceptance barriers.

## Failure and result semantics

Results use `pi.subagent.workflow.v1` with nodes in authored order. Every node
has `id`, `needs`, `state`, zero-based `wave` when attempted, `hubId`,
`nativeIsError`, and its complete native `result`. Skipped nodes have no child
result or synthetic hub entry; `skipReason` and `blockedBy` explain why they
were not launched. Counts distinguish accepted, rejected and skipped nodes.
Node acceptance states, wave numbers and real hub IDs also appear in result
text, because model providers do not generally transmit arbitrary tool details.
The parent can use those handles with the existing hub/agent URL surfaces
without deriving child identity from generated prose.

A predecessor is accepted only after native completion, with no native error
and no failed schema validation. Even `schemaMode: permissive` invalid output
is retained as a native warning but rejected as dependency evidence. Failed,
operator-killed, cancelled or schema-invalid nodes block their descendants;
an independent branch may continue. `stopReason` reports a request interruption
observed between waves. The tool result is unsuccessful if any node is rejected
or skipped for an error, not merely `not_selected`. Native progress events remain native events; terminal workflow
records provide graph IDs and hub IDs for correlation.

Completed effects are **not rolled back** when a later node fails. Worktree
conflicts remain inspectable native failures, never forced. No workflow-level
retry or automatic resume occurs. Individual node outputs and preserved
worktrees retain their existing limits; the 256 KiB request bound is not a
whole-process memory or result-size guarantee. The new mode has its own
structured details rather than the optional legacy compact text footer.

## Validation

`src/subagents/workflow/tests.rs` contains graph/admission/result unit tests and
Unix native-child fixtures for real parallel barriers, fork/join ordering,
dependency failures, permissive-schema rejection, original deadlines,
cancellation, and accepted edits flowing into downstream worktrees. Typed
handoff regressions cover escaping, exact JSON values, direct-edge admission,
combined byte limits, schema provenance, multi-source child input, native
launch-control preservation and pre-launch skips. Fixtures speak the native
process protocol without contacting a model.

`src/subagents/workflow/decision/tests.rs` adds typed comparison, invalid-evidence,
failure-precedence and nonexecution-report checks, plus native-child workflows
for conditional repair/report selection, unavailable verdicts, upfront refusal
and excluding a branch before input/workspace preparation. These tests are
authored, not executed in the implementation environment.

These Rust tests were authored, not executed in the implementation environment:
`dsr quality --tool pi_agent_rust` could not start because `dsr` was unavailable.
Compilation, formatting, Clippy, native execution and cross-platform acceptance
remain unverified. The authoritative acceptance path remains DSR; no Actions
or direct Cargo substitute is used.
