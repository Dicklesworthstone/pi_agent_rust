# Session-owned completion jobs from Eval

`__pi_complete_start` starts one explicit background completion through the same
model binding and completion validator as `__pi_complete`. It returns a native
`jobId` after admission, without waiting for generation. The job can be inspected,
waited for or cancelled through the ordinary `jobs` tool in a later cell.

This requires an owning Agent invocation with an owned privacy-snapshot factory.
Standalone Eval and the borrowed-only `execute_with_host` entry point refuse the
operation rather than retain a callback or silently weaken privacy. Ordinary
awaited completions, judgments and read-only standalone bridge calls are unchanged.
Enable `jobs` to manage the returned ID through tool calls. Every later lookup
still uses the current Agent's enabled tools, approval and session policy.

## Launch, continue working, then collect

Supply `judge_model: {provider, model}` on the launching Eval invocation, or bind
a Judge in the SDK host. The model is resolved once and frozen in the job; later
cells need no model binding just to inspect it. New completion requests do not
inherit an earlier cell's selection.

```javascript
globalThis.summaryJob = JSON.parse(await tool.call('__pi_complete_start', {
  prompt: 'Summarize the following findings: ...',
  system: 'Preserve unresolved concerns in a concise technical summary.',
  max_tokens: 512
}));
display(summaryJob.jobId);
```

That cell may finish while generation continues. A later cell uses the native
job result, not a second model request:

```javascript
const snapshot = await tool.result('jobs', {
  action: 'wait', jobId: summaryJob.jobId, timeoutMs: 5000
});
if (snapshot.isError) throw new Error('Job lookup failed');
if (snapshot.details.status === 'running') {
  display('Still running; keep the original job ID.');
} else if (snapshot.details.status !== 'exited' || snapshot.details.result.isError) {
  display(snapshot.details.result);
} else {
  display(snapshot.details.result.details.text);
}
```

Python uses the same operations and result structure:

```python
import json
summary_job = json.loads(await tool.call('__pi_complete_start', {
    'prompt': 'Extract the relevant file paths: src/api.rs needs review.',
    'schema': {
        'type': 'object', 'required': ['files'], 'additionalProperties': False,
        'properties': {'files': {'type': 'array', 'items': {'type': 'string'}}},
    },
}))
display(summary_job['jobId'])
```

```python
snapshot = await tool.result('jobs', {
    'action': 'wait', 'jobId': summary_job['jobId'], 'timeoutMs': 5000,
})
if snapshot.get('isError'):
    raise RuntimeError('Job lookup failed')
job = snapshot['details']
if job['status'] == 'running':
    display('Still running; keep the original job ID.')
elif job['status'] != 'exited' or job['result'].get('isError'):
    display(job['result'])
else:
    display(job['result']['details']['data']['files'])
```

`jobs` with `action: "wait", timeoutMs: 0` inspects without waiting. Use
`action: "cancel"` and the same `jobId` to cancel. A wait timeout does not replay
or cancel the completion. Keep the existing ID rather than launching a duplicate.
The usual background completion notice and retained artifact use the same jobs
registry as shell and subagent work; there is no second completion scheduler.

## Ownership, limits and outcomes

The start arguments are exactly the ordinary completion contract: `prompt`,
optional `system`, `max_tokens`, and `schema`. Invalid contracts fail before
attempt reservation or provider admission. Schema bounds, strict terminal JSON,
no external schema retrieval, tool-free generation and no automatic retry or
fallback are unchanged. Each start consumes one of the launching cell's shared
16 completion/judgment attempts, including starts that fail during admission.
The native jobs capacity and result-retention limits apply as well.

A fixed **60-second budget begins at background admission**, including model
resolution and generation. Queueing or polling in a later cell cannot restart
it. The originating owner's cancellation and inherited deadline remain upper
bounds. This explicit job budget is separate from the launching cell's runtime
watchdog; no existing cell wait is paused or granted more time. Launch preparation
is still inside that cell's watchdog. Native synchronous operations remain
cooperative; this is not a preemptible CPU or filesystem-I/O sandbox.

Jobs capture the original session's shutdown generation and check the live
session around preparation. The model, parsed request and owned privacy snapshot
may outlive the launching cell; its live projection, Agent callback, interpreter,
and mutable tool registry do not. Generation uses the frozen privacy policy.
A result that would change under that policy is withheld, not published with a
stale `schemaValid: true` assertion. The jobs runtime also projects its complete
output before retention, artifacts or notices.

The acknowledgment has schema `pi.completion.background.v1`, a `jobId`, the
expected result schema and `budgetSeconds`. Successful settled results retain
the ordinary `pi.completion.v1` details, including exact text, optional validated
data, model identity and reported usage. Failed/cancelled/timed-out jobs retain
an unsuccessful native result, never a fabricated answer or an automatic retry.

The launching cell's `details.judgments.attempts` records
`operation: "completion_background"`. `launching` means admission did not finish;
`background` includes the admitted `jobId`, not a promise that generation is
still running. The eventual result and usage belong to the job, not an invented
successful receipt in the earlier cell. Receipts contain no prompts or answers.
Unknown usage remains unknown; these are not aggregate session cost accounting
or a durable billing ledger.

Reset or per-call kernel disposal loses language variables, **not admitted jobs**.
Session shutdown cancels unfinished work through native jobs cleanup. An explicit
owner cancellation can cancel it too. A cell timeout/error after launch does not
undo work already admitted; inspect `jobs list` and native launch receipts when
an acknowledgment could not reach guest code. Session switching cannot grant a
new session access to old job IDs. There is no credential or ownership embedded
in the ID itself.

This is the native start/result lifecycle, not full OMP API compatibility. The
high-level OMP `completion()` handle class, completion-tier selection and OMP's
wait-watchdog suspension are not supplied by this increment.

## Validation

`src/eval/judgment/background/tests.rs` contains controlled-provider tests for
native acknowledgment/settlement, both Python and QuickJS cross-cell paths,
reset, typed results, current host policy, session changes, cancellation,
shutdown isolation, deadlines, attempt bounds and frozen privacy. These Rust
regressions are authored, not executed in the implementation environment.
`dsr quality --tool pi_agent_rust` is authoritative; it could not start because
DSR was absent. Documentation/syntax/source checks are not native acceptance.
