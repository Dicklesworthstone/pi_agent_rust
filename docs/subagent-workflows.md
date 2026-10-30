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

Each node's `call` contains ordinary per-task fields: `agent`, `task` or `taskFile`,
`model`, `cwd`, `isolation`, `isoApply`, `outputSchema` and `schemaMode`. All call shapes and
workspace options are validated up front. Agent discovery and all effective
schema compilation also finish before the first child. Native filesystem
conditions are still checked at execution; this is not a transactional dry run.
Mixed modes and unknown fields are refused rather than silently ignored.

`taskFile` follows the [file instruction contract](subagent-definitions.md#instructions-from-a-file).
Every node's file is captured before the first wave or background acknowledgment,
even for a conditional branch or later fan-out. An unreadable, invalid or
oversized source refuses the whole graph without launching a valid prefix.
The captured body, plus any typed inputs or fan-out item, must still fit the
64 KiB expanded-task budget. Every executed instance reports the submitted
`taskFile` and the SHA-256 of the original captured source bytes; its public
`result.task` displays the reference. Predecessor data cannot replace that source
or cause a later filesystem read.

An explicit `model` uses the [per-call selector contract](subagent-definitions.md#per-call-model-selection).
Every node's provider, model and optional thinking suffix is resolved before
the first wave, even for a node whose dependencies have not run. A bad selector
refuses the graph before any work starts. Dependency inputs and fan-out items
only supply task data; they cannot change the frozen model or effort.

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
when another prerequisite was intentionally not selected. By default, a downstream
task requiring **both** alternative branches will not run. An explicit selected
join, described below, allows those alternatives to converge.

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

### Converging selected branches

Set `join: "selected"` on a node that must continue after whichever alternative
actually ran. For example, append this node to the review/repair/report workflow:

```json
{
  "id": "finish",
  "needs": ["repair", "report"],
  "join": "selected",
  "call": {"agent": "reviewer", "task": "Inspect the final accepted workspace and summarize the outcome."}
}
```

The default `join: "all"` still requires every predecessor to be accepted.
`selected` requires **at least one accepted predecessor**, with every other
predecessor intentionally `not_selected`. Any rejected, error-skipped or pending
predecessor blocks the join. An accepted sibling cannot hide failed execution,
invalid JSON, a missing input, cancellation or a worktree conflict. If all
predecessors were not selected, the join is also `not_selected` with
`skipReason: "no_selected_dependencies"`; no child is invented for it.

This is not an any-success race: all declared edges remain in the topological
graph, and the existing wave/patch-acceptance barrier is unchanged. Multiple
selected branches must **all** be accepted before the join runs. A joined worktree
captures its baseline after accepted predecessors settle, so it sees their
applied changes. `keep` and `drop` retain their ordinary non-writeback semantics.

An explicitly supplied `join` requires nonempty `needs`. Null, unknown values and
policy on a single/parallel/chain delegation are refused. The join only changes
dependency admission: `when` and `inputs` still require their exact schema-valid
source data. Selecting data from an unselected alternative is an error, not null,
an omitted input or substitution from another branch. Put shared data in a
predecessor that always runs, or bind known selected inputs within each branch.
The original timeout and cancellation checks still gate the converged task;
joining never starts a fresh request or retries either branch.

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
or skipped for an error, not merely `not_selected`. Native progress events remain
native events; terminal workflow records provide graph IDs and hub IDs for correlation.

Completed effects are **not rolled back** when a later node fails. Worktree
conflicts remain inspectable native failures, never forced. No workflow-level
retry or automatic resume occurs. Individual node outputs and preserved
worktrees retain their existing limits; the 256 KiB request bound is not a
whole-process memory or result-size guarantee. The new mode has its own
structured details rather than the optional legacy compact text footer.

## Data-driven fan-out

`forEach` turns a validated discovery result into bounded native child tasks.
The node's `call` remains a single authored policy; each item becomes a literal
named value in the same `<workflow-inputs>` block used by `inputs`.

```json
{
  "workflow": [
    {
      "id": "discover",
      "call": {
        "agent": "scout",
        "task": "Find the packages affected by the requested change.",
        "outputSchema": {
          "type": "object", "required": ["packages"],
          "properties": {"packages": {"type": "array", "items": {"type": "string"}}}
        }
      }
    },
    {
      "id": "audit", "needs": ["discover"],
      "forEach": {"from": "discover", "pointer": "/packages", "as": "package", "maxItems": 16},
      "call": {"agent": "reviewer", "task": "Audit the assigned package for correctness."}
    },
    {
      "id": "finish", "needs": ["audit"],
      "call": {"agent": "reviewer", "task": "Inspect the final workspace after all package audits settle."}
    }
  ],
  "concurrency": 4,
  "timeoutSeconds": 300
}
```

The source must be a direct `needs` predecessor with accepted, schema-validated
data. `pointer` selects a JSON array; null, a missing value, or a non-array is
an error, never an empty list. `as` is an unused input name and cannot replace
one of the node's shared `inputs`. Each item retains its complete JSON type and
literal contents; duplicate items are separate work, not silently deduplicated.
The task, agent, model, tools, cwd, isolation, schema and deadline do not come
from item data. Data remains untrusted model context, not a sandbox.

`maxItems` is required, from 1 to 32. The sum of all declared maxima and ordinary
calls must be at most 128 logical task instances, checked before any launch.
This is a worst-case bound, even for branches that later are not selected.
The native one-time schema-correction retry can start a replacement process per
applicable instance; it does not obtain a new task or time allowance. The
existing 32-node and 256 KiB request bounds remain unchanged.

All expanded tasks across a wave share the requested concurrency limit, one
captured baseline per root and authored-node/input-item patch acceptance order.
This is not one independent pool per node. Every item prompt must fit the
existing 64 KiB limit before **any** item from that node is queued. Excessive
lists and late-item preparation errors skip the whole node without launching a
prefix; independent nodes can continue. `when` is evaluated before expansion,
so an unselected node never prepares item inputs or workspaces.

An empty array completes the node with zero children. Otherwise every item
must pass native acceptance for the node to be accepted. A rejected item,
including a permissive schema warning, blocks dependants while preserving
successful sibling results and any accepted edits. The ordinary wave barrier
still applies; there is no rollback, early join, cancellation override or replay.

Mapped nodes have `fanout.itemCount` and input-ordered `fanout.items`, each with
`index`, acceptance `state`, real `hubId`, `nativeIsError` and native `result`.
The container has no synthetic child identity or native result; item states and
handles also appear in tool text. Native progress/hub/revival records retain
each expanded prompt; terminal `result.task` retains the authored assignment.
An empty node records its admitted wave but does not increment `wavesExecuted`
unless that wave actually submitted native tasks. Per-item native output bounds
still apply; the expansion cap is not a total result-memory guarantee.

This is bounded fresh-child fan-out, not OMP's persistent Eval `workpool` API.

### Typed fan-in and subsequent decisions

Give a mapped node's call an `outputSchema` (or inherit its agent definition's
schema) to validate **each worker result**. When every worker passes, the node's
typed data is an array of those values in original item order. Downstream
`inputs`, `when`, and `forEach` address that array through the same data path as
ordinary nodes. The schema applies to each element, not to the assembled array.

For example, these two nodes can follow the `discover` node above:

```json
[
  {
    "id": "audit", "needs": ["discover"],
    "forEach": {"from": "discover", "pointer": "/packages", "as": "package", "maxItems": 16},
    "call": {
      "agent": "reviewer", "task": "Audit the assigned package and report its findings.",
      "outputSchema": {
        "type": "object", "required": ["package", "findings"],
        "properties": {
          "package": {"type": "string"},
          "findings": {"type": "array", "items": {"type": "string"}}
        }
      }
    }
  },
  {
    "id": "summarize", "needs": ["audit"],
    "inputs": {"reports": {"from": "audit", "pointer": ""}},
    "call": {"agent": "reviewer", "task": "Summarize the supplied package audit reports."}
  }
]
```

Here `reports` is an array such as
`[{"package":"core","findings":[]},{"package":"ui","findings":["..."]}]`.
Empty pointer selects the entire array; `/0/findings` selects one item's field.
Null elements, escaped keys, order and duplicate values remain intact. An empty
map publishes the known empty array without inventing schema-validated child
results. Any source used for a typed handoff still requires an effective item
schema before the workflow launches, including an eventually empty source.

The complete aggregate is bounded to **256 KiB of JSON-encoded data**, counted
before copying. `fanout.data` is present only when available;
`fanout.dataUnavailable` is null or explains `not_settled`, `rejected_item`,
`unvalidated_item`, or `result_limit`. A failed or schema-free item cannot create
a partial successful array. If all native tasks succeeded but the combined data
is too large, the node remains accepted as work; its aggregate is unavailable,
and a downstream data consumer skips with an explicit unavailable-data error.
Individual native results remain intact in `fanout.items`; no data is silently
truncated or reconstructed from their prose. Aggregate availability is also
reported in the model-visible result text.

The aggregate is an additional bounded snapshot in this invocation only, not a
persistent cache. The stricter 64 KiB expanded-task limit still governs a later
handoff, so an available aggregate may be too large to pass whole: select a
smaller field explicitly. Nested maps keep the same graph-wide declared-call
ceiling, global concurrency and original deadline. They cannot launch children
early, suppress failed dependencies, or bypass selected-branch admission.

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

The same suite also covers the full two-predecessor state matrix, malformed join
policy, mandatory data provenance, retained topological edges, native convergence,
all-unselected versus mixed-failure outcomes, cancellation, missing branch inputs
and an isolated joined task reading its predecessor's accepted changes while the
parent index/HEAD stay unchanged. None of these Rust tests were executed here.

`src/subagents/workflow/fanout/tests.rs` adds unit and native-child scenarios for
discovered-item parallelism, full-list admission, global expansion bounds,
conditional exclusion, worktree joins, typed reduction, further mapping,
aggregate limits and cancellation. Its subprocess fixtures do not call a model.

These Rust tests were authored, not executed in the implementation environment:
`dsr quality --tool pi_agent_rust` could not start because `dsr` was unavailable.
Compilation, formatting, Clippy, native execution and cross-platform acceptance
remain unverified. The authoritative acceptance path remains DSR; no Actions
or direct Cargo substitute is used.
