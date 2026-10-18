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
