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

## Failure and result semantics

Results use `pi.subagent.workflow.v1` with nodes in authored order. Every node
has `id`, `needs`, `state`, zero-based `wave` when attempted, `hubId`,
`nativeIsError`, and its complete native `result`. Skipped nodes have no child
result or synthetic hub entry; `skipReason` and `blockedBy` explain why they
were not launched. Counts distinguish accepted, rejected and skipped nodes.

A predecessor is accepted only after native completion, with no native error
and no failed schema validation. Even `schemaMode: permissive` invalid output
is retained as a native warning but rejected as dependency evidence. Failed,
operator-killed, cancelled or schema-invalid nodes block their descendants;
an independent branch may continue. `stopReason` reports a request interruption
observed between waves. The tool result is unsuccessful if any node is rejected
or skipped. Native progress events remain native events; terminal workflow
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
cancellation, and accepted edits flowing into downstream worktrees. Fixtures
speak the native process protocol without contacting a model.

These Rust tests were authored, not executed in the implementation environment:
`dsr quality --tool pi_agent_rust` could not start because `dsr` was unavailable.
Compilation, formatting, Clippy, native execution and cross-platform acceptance
remain unverified. The authoritative acceptance path remains DSR; no Actions
or direct Cargo substitute is used.
