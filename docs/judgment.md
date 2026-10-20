# Native typed judgments

`pi::judgment::Judge` is an unstable library API for small, typed decisions.
Bind a `Provider` and `StreamOptions`, validate a `JudgmentRequest`, then await
`Judge::evaluate`. It makes one stateless, tool-free provider request. It does
not select a model, add ambient credentials, retry or fail over.

Requests contain exactly `state` and `questions`. State is nonempty text, an
object or an array. Questions are keyed by ID and have `type`, nonempty
`instructions`, and the following criteria:

- `bool`: optional `{ "true": "rubric", "false": "rubric" }`.
- `choice`: 2-16 labels mapped to rubric text or null.
- `score`: 2-16 ordered rubric strings, lowest first.

The limits are 16 questions, 128 KiB encoded input, 32 JSON levels, 65,536
values, 64-byte IDs/choice labels, and 4,096-byte rubric strings. Images,
unknown request fields and model-selection directives are not supported.

The model returns probabilities. Native validation requires all questions,
all choice labels and all score levels, with no duplicate or unknown keys.
Values must be finite numbers from zero to one. Distributions must sum to
one within 0.000001; accepted rounding is normalized. Missing answers,
booleans or numeric strings in place of probabilities, markdown wrappers,
truncated responses, tool calls and failed/absent terminal events are errors.
There is no partial-success or heuristic fallback verdict.

The result includes `schema: "pi.judgment.v1"`, `answers`, the producing
`provider`, `modelId`, qualified `model`, `api`, and reported terminal `usage`.
Catalog pricing is applied when supplied by the provider. Bool answers contain
`{type: "bool", bool: probability}`. Choice answers contain the highest-
probability `choice` (lexical ties), `probabilities` and `confidence`. Score
answers contain the weighted zero-based level index, probabilities keyed by
level index, and confidence. Confidence is one minus normalized entropy. These
are model estimates and distribution concentration, not calibrated accuracy.

The entire payload and system instruction pass the auxiliary privacy floor
before provider admission. Configured patterns extend it; block mode refuses.
An invocation may supply its live Agent projection internally. Redaction must
not change question IDs, answer types or choice labels. The original state and
rubric are never mutated. Provider failures use payload-free diagnostics.

A 60-second ceiling covers connection and response collection, within inherited
request cancellation/deadline authority. Responses are limited to 32 KiB and
4,096 requested output tokens (or the caller's lower token ceiling).

Usage is returned on successful calls; this API does not yet journal failed
attempts or update aggregate Agent session cost. It does not implement TypeSafe,
a role fallback chain, background batches, caching, or a retained workpool.

## Judgments inside eval

Both native eval kernels support the `__pi_judge` bridge operation. It returns
JSON text containing the same complete typed verdict, never a preview or a
retained task handle. For example, this JavaScript cell can be supplied to eval:

```json
{
  "kernel": "js",
  "judge_model": {"provider": "your-registered-provider", "model": "your-model-id"},
  "code": "JSON.parse(await tool.call('__pi_judge', {state: {change: 'adds regression tests'}, questions: {safe: {type: 'bool', instructions: 'Does this change improve test coverage?'}}})).answers.safe.bool"
}
```

Python uses `json.loads(await tool.call('__pi_judge', {'state': evidence,
'questions': questions}))`. An SDK host may instead construct
`EvalTool::new(cwd).with_judge(judge)` with an already authenticated `Judge`.
Neither path adds general tool re-entry: the normal bridge remains restricted
to read/grep/find/ls, and nesting the operation inside `tool.result` is denied.

`judge_model` selects exactly one model from the host registry and applies
trusted configuration, configured disabled providers and normal host credential
resolution. Its only fields are `provider` and `model`, each at most 256 bytes;
guest-supplied endpoints, keys and headers are refused. Unknown models or missing
credentials fail without fallback. Selection is per cell, not retained with
interpreter variables, and cannot override a host-bound Judge. Invalid selection
fails before waiting for a kernel or applying a reset. Cells that do not request
a judgment do not load judgment configuration or credentials.

At most **16 judgment attempts per cell** are admitted, including failed attempts.
The existing eval deadline, cancellation and original session owner cover model
resolution and response collection. A shorter cell deadline wins over the
Judge's 60-second ceiling. No generation task is detached; local credential
resolution may finish after its wait is dropped, but cannot dispatch a model
request. Both session and per-call kernels use the same native path.

Agent-owned eval calls borrow the current auxiliary privacy projection for the
whole cell. This also screens dynamic evidence read from files inside the
kernel, not just the original eval source. The projection is never stored in a
shared tool or retained interpreter. Standalone calls use the bound Judge's
auxiliary privacy policy.

Native `details.judgments` metadata has schema `pi.eval.judgments.v1` and records
admitted attempts even when guest code discards the answer or catches its error.
Completed attempts include provider/model identity and reported usage. Failed
or interrupted attempts have `usage: null` and `usageKnown: false`, not an
invented zero cost. Python host errors append the bounded receipt JSON under
`EVAL_JUDGMENTS`; ordinary guest failures retain result metadata. These receipts
do not update aggregate Agent cost and are not a durable billing journal: a
caller dropping the entire eval future cannot receive its result or receipts.

The module includes request/response validation and controlled provider-stream
unit fixtures, plus native Python/QuickJS eval integrations with controlled
providers in `src/eval/judgment/tests.rs`. They are not live-model tests.
The authoritative acceptance
command remains `dsr quality --tool pi_agent_rust`.

## Bounded judgment batches inside eval

`__pi_judge_batch` evaluates several independent states against one shared
rubric, using the same native Judge as `__pi_judge`. It is an invocation-bound
batch, not OMP's background batch handle or a retained workpool. Requests contain
`states`, `questions`, and optionally `concurrency` (1-8; default 4). Use either
an array of 1-16 states or an object with 1-16 literal keys. Object keys must be
nonempty, at most 128 UTF-8 bytes, and contain no control characters. Each state
and the shared questions follow the ordinary single-judgment contract.

```json
{
  "kernel": "js",
  "judge_model": {"provider": "your-registered-provider", "model": "your-model-id"},
  "code": "globalThis.reviews = JSON.parse(await tool.call('__pi_judge_batch', {states: [{change: 'adds regression tests'}, {change: 'removes input checks'}], questions: {safe: {type: 'bool', instructions: 'Does the change improve robustness?'}}, concurrency: 2})); display({allOk: reviews.allOk, items: reviews.items});"
}
```

Python uses the same bridge and native behavior:

```python
import json
reviews = json.loads(await tool.call('__pi_judge_batch', {
    'states': {'api': {'change': 'adds validation'}, 'ui': {'change': 'adds tests'}},
    'questions': {'safe': {'type': 'bool', 'instructions': 'Does this improve robustness?'}},
    'concurrency': 2,
}))
display({'allOk': reviews['allOk'], 'items': reviews['items']})
```

The response has schema `pi.eval.judgment-batch.v1`, `total`, `succeeded`,
`failed`, `allOk`, and `items`. Every item has its original `key` and `ok`.
Array keys are zero-based numeric indices; array results retain input order
regardless of completion order. Object keys are preserved literally, not parsed
as paths or expressions; object traversal order is not a scheduling contract.
A successful item has the complete native `verdict`, including `answers` and
reported usage. A failed item has a payload-free `error`, never a substitute
verdict. Other items continue after an ordinary item failure. Check `allOk` or
each item's `ok` before using results. The cell may deliberately process a
successful subset, so a handled partial batch does not itself mark the whole
eval tool result unsuccessful. Failed attempts remain visible in host receipts.

The complete batch shape and every state/rubric are validated before credential
resolution or provider admission. Both encoded input and the sum of expanded
state/rubric requests are capped at 256 KiB; the single-request limit still
applies. The completed result is capped at 1 MiB. There is no truncation,
implicit splitting, provider fallback or automatic retry. Syntax admission is
all-or-nothing; privacy screening still occurs per item immediately before its
ordinary native request. Thus a later privacy refusal can coexist with earlier
successful results, but the refused payload is never sent to the provider.

Single calls and batch items **share the existing 16-slot cell allowance**.
The entire batch reserves its slots before any asynchronous work. A batch that
does not fit the remaining allowance is rejected without submitting a prefix.
Reserved but unstarted items keep their slots; a caught failure cannot reclaim
capacity or reset the cell budget. Concurrency bounds active evaluations in
this batch, not all calls across unrelated EvalTool instances.

The original cell deadline continues to run during queueing, model resolution
and all evaluations. It is not paused or extended for a batch. Cancellation,
conversation changes and dropped calls use the existing owned bridge; no
background task or result handle survives. Cancellation does not return a
partially completed collection as a complete batch, replay requests, or undo
completed requests and their possible costs.

`details.judgments.attempts` adds `batch` (the first reserved attempt number)
and zero-based `item` correlation. Keys, states and rubrics are not copied into
receipts. `not_started` means a reserved item was never admitted to native
evaluation; `incomplete` means it entered evaluation but no terminal outcome
was retained. Neither proves a provider request was sent or billed. Completed
and failed entries retain the existing usage semantics. Interrupted batches
may contain all four statuses. A binding failure leaves `not_started` entries
with a sanitized `admissionError`. This is not a durable billing ledger.

Batch regressions live in `src/eval/judgment/batch/tests.rs`, covering admission,
shared budgets, actual concurrent controlled-provider streams, out-of-order
completion, privacy, partial failures, cancellation, and the public Python and
QuickJS EvalTool paths. They were authored, not compiled or executed in the
implementation environment: `dsr quality --tool pi_agent_rust` could not start
because `dsr` was unavailable. No independent fixture check is native acceptance.
