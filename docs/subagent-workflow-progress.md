# Recoverable background workflow checkpoints

A `subagent` graph submitted with `async: true` now publishes a checkpoint after
each fully settled dependency wave. This makes accepted earlier work inspectable
while later children run. Cancellation and task timeout retain the last screened
checkpoint in the existing jobs result instead of discarding all graph evidence.

This is a checkpoint of the native workflow runner, not a second scheduler,
automatic restart, rollback, or durable resume mechanism. Foreground workflows
and non-workflow background delegations keep their existing result contracts.

## Submit and inspect

```json
{
  "async": true,
  "timeoutSeconds": 120,
  "workflow": [
    {
      "id": "inspect",
      "call": {
        "agent": "scout",
        "task": "Identify the relevant source files and return a JSON array.",
        "outputSchema": {"type": "array", "items": {"type": "string"}}
      }
    },
    {
      "id": "review",
      "needs": ["inspect"],
      "inputs": {"files": {"from": "inspect", "pointer": ""}},
      "call": {
        "agent": "reviewer",
        "task": "Review the supplied files and explain the important findings."
      }
    }
  ]
}
```

The acknowledgment still uses `pi.subagent.background.v1` and returns `jobId`.
Use the ordinary `jobs` tool with `action: "wait"`, that `jobId`, and
`timeoutMs: 0` for a nonblocking observation. In an owning JavaScript Eval cell:

```javascript
const observation = await tool.result("jobs", {
  action: "wait", jobId: workflowJobId, timeoutMs: 0
});
if (observation.isError) throw new Error("Job lookup failed");
const job = observation.details;
const envelope = job.result?.details;
if (envelope?.schema === "pi.host_job.progress.v1" ||
    envelope?.schema === "pi.host_job.interrupted.v1") {
  const checkpoint = envelope.partialResult.details;
  display({status: job.status, checkpoint});
} else {
  display(job.result); // Null before the first checkpoint, or a final result.
}
```

Every lookup still uses current host permissions and session ownership. An ID
does not grant another conversation access. Inspecting a job does not launch
children, replay input, renew the original deadline, or consume a completion
notice. Updates replace the live observation; only terminal settlement generates
the normal completion notice and artifact.

## Evidence and state

The inner `partialResult` is a complete native `ToolOutput`. Its details use
`pi.subagent.workflow.progress.v1`, with:

- `complete: false`, a monotonically increasing `checkpoint` number, and the
  zero-based `settledThroughWave` index.
- `wavesExecuted`, which counts actual submitted waves, plus accepted, rejected,
  skipped, not-selected and pending node counts.
- `nodes` in authored order, retaining native results, typed data, conditions,
  actual hub IDs, and the complete per-item records for a settled fan-out.

Empty fan-outs and excluded waves do not invent child processes or executed
waves. A failed node may coexist with an independently running branch; a live
checkpoint is not itself a terminal error. The normal final workflow report
still determines overall success or failure.

Checkpoints happen only after the native runner has settled the entire wave's
schema checks and ordered worktree acceptance. A dependant can therefore consume
accepted predecessor data and edits exactly as before. No provisional child text
is promoted to accepted evidence by the checkpoint mechanism.

**Pending is a state at the recorded checkpoint, not proof that a node never
started.** The next wave may already be running when the checkpoint is read.
An interrupted wave may have performed effects not represented by the last
checkpoint, including writes accepted just before publication lost a race.
Do not automatically replay unresolved nodes. Inspect the workspace and native
hub evidence before choosing any continuation.

## Interruption, privacy and bounds

A running job exposes `pi.host_job.progress.v1`. Cancellation or deadline expiry
produces an unsuccessful `pi.host_job.interrupted.v1` wrapper retaining its last
confirmed `partialResult`. Completed native work is not rolled back. A normally
settled graph replaces the progress wrapper with its unchanged
`pi.subagent.workflow.v1` report. Generic failures, panic, or failed publication
may have no partial result; no missing result is synthesized.

The owned privacy policy is captured at admission. Checkpoints must survive that
projection unchanged: redacting a typed result cannot leave `schemaValid: true`
claiming validation of different data. A privacy or serialization failure
withdraws older progress, reports failure, and prevents subsequent waves from
starting. Already completed effects remain. The raw rejected checkpoint is not
retained in jobs artifacts or completion notices.

The complete checkpoint is counted before copying its native result data and
is capped at 768 KiB. Existing jobs serialization, auxiliary privacy and total
retention budgets also apply and can impose a lower effective limit. Evidence
is not truncated, coerced or partially dropped to fit. Graph size, fan-out,
concurrency, worktree rules and retry policy are unchanged.

The jobs monitor is bound to the original monotonic delegation deadline before
asynchronous owner lookup. Publication handoff and later polling cannot extend
it. State survives Eval cell completion and kernel reset through the native
jobs registry, not a retained interpreter. It does not survive host-process
restart, guarantee retention after eviction, or constitute a durable journal.

## Validation

Eight Unix Rust regressions in `src/subagents/workflow/progress/tests.rs` cover
accepted worktree evidence, cancellation and child reaping, the original
deadline, independent failure, fan-out/branch state, empty expansions, privacy
withdrawal, and bounded complete output. They are authored, not compiled or
executed on the implementation host. The required
`dsr quality --tool pi_agent_rust` command was unavailable (exit 127).
Fixture syntax and protocol-format checks are not Rust runtime acceptance.
