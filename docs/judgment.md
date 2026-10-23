# Native typed judgments

`pi::judgment::Judge` is an unstable library API for small, typed decisions.
Bind a `Provider` and `StreamOptions`, validate a `JudgmentRequest`, then await
`Judge::evaluate`. It makes one stateless, tool-free provider request. It does
not select a model, add ambient credentials, retry or fail over.

The text contract parsed by `JudgmentRequest::parse` contains exactly `state`
and `questions`. State is nonempty text, an
object or an array. Questions are keyed by ID and have `type`, nonempty
`instructions`, and the following criteria:

- `bool`: optional `{ "true": "rubric", "false": "rubric" }`.
- `choice`: 2-16 labels mapped to rubric text or null.
- `score`: 2-16 ordered rubric strings, lowest first.

The limits are 16 questions, 128 KiB encoded input, 32 JSON levels, 65,536
values, 64-byte IDs/choice labels, and 4,096-byte rubric strings. Unknown text
request fields and model-selection directives are not supported. Validated
native image evidence can be attached separately with `with_images`, as
described below; its encoded bytes do not consume this text allowance.

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

The entire text payload and system instruction pass the auxiliary privacy floor
before provider admission. Configured patterns extend it; block mode refuses.
An invocation may supply its live Agent projection internally. Redaction must
not change question IDs, answer types or choice labels. The original state and
rubric are never mutated. Provider failures use payload-free diagnostics.

A 60-second ceiling covers connection and response collection, within inherited
request cancellation/deadline authority. Responses are limited to 32 KiB and
4,096 requested output tokens (or the caller's lower token ceiling).

Usage is returned on successful calls; this API does not yet journal failed
attempts or update aggregate Agent session cost. It does not implement TypeSafe,
a role fallback chain, or caching. Eval can also launch the same native
requests as [retained background model jobs](#retained-background-model-jobs).

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
Neither auxiliary operation grants new tool rights. Agent-owned cells use the
owning Agent's enabled tools and current policy; standalone embeddings retain
the local read/grep/find/ls bridge. `tool.result` does not select this auxiliary
model operation; it projects results from the ordinary tool dispatcher.

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

## Image evidence for single and batch judgments

Judgments accept bounded PNG, JPEG, GIF and WebP evidence as native image
content blocks. The provider receives the screened state and questions in one
text block, followed by the images in supplied order. Typed answers and verdict
validation are unchanged. A request without images retains the existing text
wire shape. Image support does not add image input to `__pi_complete`; general
completions remain text-only.

For the library API, parse the text request first and attach native
`Vec<pi::model::ImageContent>` values with
`JudgmentRequest::with_images(images)?`. A later `with_images` call replaces
the prior collection after validating the replacement. `has_images()` reports
whether image capability is required; `image_bytes()` reports the sum of the
encoded image payload lengths, excluding the text request. For example, with
provider/options and native images supplied by the host:

```rust
let judge = Judge::new(provider, options)
    .with_image_support(model_supports_images);
let request = JudgmentRequest::parse(text_request)?.with_images(images)?;
let verdict = judge.evaluate(request).await?;
```

Image support must be declared by trusted host configuration. A `Judge` defaults
to refusing image requests until the host binds it with
`with_image_support(true)`. Eval's `judge_model` binding derives this capability
from the selected catalog entry's image input support. Guest request fields
cannot enable it. An unsupported binding fails with
`PI_JUDGE_IMAGE_UNSUPPORTED` before image path reads or provider admission;
there is no fallback or silent removal of images. An empty image list is an
ordinary text request.

The eval `__pi_judge` request adds an optional `images` array alongside `state`
and `questions`. Each entry is either a local path string or an embedded object:

```javascript
const verdict = JSON.parse(await tool.call('__pi_judge', {
  state: {screen: 'checkout confirmation'},
  questions: {
    clear: {type: 'bool', instructions: 'Does the screen clearly confirm success?'}
  },
  images: ['artifacts/confirmation.png']
}));
display(verdict.answers.clear.bool);
```

The same `images` key is accepted by Python's dictionary request. Embedded
images use exactly `{type: 'image', data: base64Text, mimeType: 'image/png'}`
with the appropriate supported MIME type. Optional `detail` may only be
`'auto'`; other detail settings are rejected rather than ignored. Only the
exact MIME strings `image/png`, `image/jpeg`, `image/gif` and `image/webp` are
accepted. URLs, data URLs, SVG, unknown image object fields, noncanonical
base64 and a MIME that disagrees with the image envelope are refused.

Each request accepts at most **4 images**, at most **4,718,592 decoded bytes per
image** (6 MiB of canonical base64), and at most **8 MiB of encoded image bytes
in total**. Both canvas dimensions must be nonzero and at most 16,384 pixels;
the canvas may contain at most 16,777,216 pixels. These are envelope and canvas
checks, not a full raster decode, animation frame limit or guarantee that a
downstream decoder will accept the payload. Native admission preserves the
accepted image bytes; it does not resize, transcode or strip image metadata.

Local paths require the current cell's Agent tool host. Each path is passed to
that host's enabled `read` tool under its current policy and captured session
owner. A successful read must return exactly one native image; descriptive text
from the read is not added to the judgment. The normal read tool may apply its
own supported image handling before this admission step. Paths are limited to
4,096 UTF-8 bytes and cannot be blank or contain control characters. Every path
read consumes the cell's existing 64-call host-tool allowance and produces a
native tool receipt. The complete required read count is checked before reads
begin. A standalone Judge or eval binding without that Agent host must supply
embedded images; there is no direct filesystem fallback for this operation.

Text privacy checks still cover the full state, question IDs, labels, rubrics
and system instruction before provider admission. Encoded image bytes are kept
outside the text privacy projection and its budget. Image pixels and embedded
metadata are **not inspected or redacted for secrets**. A text privacy refusal
prevents the whole judgment, including its images, from being sent. Image
validation and read failures return named, payload-free diagnostics; neither
image bytes nor image paths are copied into judgment or tool receipts.

`__pi_judge_batch` adds an optional `images` object whose keys select individual
states. For object-shaped states, use the exact literal state keys. For an
array of states, use canonical zero-based index strings such as `'0'` and
`'1'`. Unknown keys, noncanonical array indices and non-object image maps are
rejected. Omitted keys supply no images for that state. For example:

```python
reviews = json.loads(await tool.call('__pi_judge_batch', {
    'states': {'before': {'version': 'before'}, 'after': {'version': 'after'}},
    'questions': {'clear': {'type': 'bool', 'instructions': 'Is confirmation clear?'}},
    'images': {
        'before': ['artifacts/before.png'],
        'after': ['artifacts/after.png'],
    },
    'concurrency': 2,
}))
```

The batch has a separate **16 MiB aggregate encoded image allowance** as well
as every per-request image limit. All image descriptors are validated and all
required paths are read before the first provider request. An image preparation
failure refuses the whole batch without sending an earlier state. Once batch
slots are reserved, an image preparation failure leaves them reserved with a
sanitized admission error. Text privacy screening remains per item immediately
before its native evaluation. Neither attachment preparation nor generation
extends the original cell deadline or detaches work from its owner.

Core image regressions are in `src/judgment/tests.rs`; bridge, path-policy and
batch image fixtures are in `src/eval/judgment/images/tests.rs`. They use
controlled hosts and providers and do not establish live-model compatibility.

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
`states`, `questions`, and optionally `concurrency` (1-8; default 4) and the
per-state `images` map described above. Use either
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

The foreground response has schema `pi.eval.judgment-batch.v1`, `total`, `succeeded`,
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
resolution or provider admission. Both encoded text input and the sum of
expanded state/rubric requests are capped at 256 KiB; the single-request limit still
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

## General one-shot completions inside eval

`__pi_complete` uses the same explicit auxiliary model binding as judgments,
without requiring a bool/choice/score rubric. It accepts `prompt` and optional
`system` and `max_tokens`. It returns JSON with schema `pi.completion.v1`, the
terminal `text`, provider/model/API identity and reported `usage`. It never
replays the conversation or advertises or executes tools. This is an awaited
completion within the current cell, not OMP's retained completion handle.

```json
{
  "kernel": "js",
  "judge_model": {"provider": "your-registered-provider", "model": "your-model-id"},
  "code": "globalThis.summary = JSON.parse(await tool.call('__pi_complete', {prompt: 'Summarize the following findings: ...', system: 'Write a concise technical summary.', max_tokens: 512})); display(summary.text);"
}
```

Python uses the same native path:

```python
import json
summary = json.loads(await tool.call('__pi_complete', {
    'prompt': 'Summarize the following findings: ...',
    'system': 'Write a concise technical summary.',
    'max_tokens': 512,
}))
display(summary['text'])
```

`judge_model` is the existing per-cell binding shared with `__pi_judge` and
`__pi_judge_batch`; an SDK host can instead use `with_judge`. There is no
additional model selection inside a completion, no ambient model fallback,
and no guest-supplied endpoint, key, header, history or tool list. The native
library equivalent is `Judge::complete(CompletionRequest::parse(input)?)`.

Prompt and system must be nonempty strings, at most 64 KiB and 16 KiB of UTF-8
respectively. Whitespace and literal content are otherwise preserved; prompt
text is not a template or executable input. Omitted system uses a tool-free
answer instruction. Both fields are screened together through the invocation's
borrowed privacy projection, or the binding's auxiliary privacy floor for a
direct library call. The callback is not retained between cells. Oversized or
malformed projections fail before provider admission, rather than clipping
unscreened content.

`max_tokens` is an integer from 1 to 4096 and can only lower the bound host
ceiling. The terminal response is capped at 32 KiB. Only a clean `stop` event
and terminal message supply an answer: missing completion events, truncation,
tool-use stops, tool calls, empty text and provider errors fail. There is no
heuristic partial answer or automatic replay. The ordinary 60-second auxiliary
request limit remains inside the original eval deadline; waiting does not
pause or renew it. Dropping or cancelling the call drops the active request.
Completed provider work and possible costs cannot be undone.

Completions, individual judgments and all batch items share **16 attempts per
cell**. A completion reserves one slot before any await. Invalid input and an
unbound request do not allocate a slot. An admitted failed or interrupted
request retains its slot. The existing `details.judgments.attempts` container
also records completions with `operation: "completion"`; prompts and returned
text are not copied into those receipts. They survive caught guest failures
or discarded answers. Unknown usage stays null, not an invented zero cost;
these receipts are neither aggregate session accounting nor a durable ledger.
Standalone `tool.result('__pi_complete', ...)` remains denied by the local
bridge. Agent-owned tool calls retain their native host policy; they do not
select this auxiliary completion operation. Completion input cannot expand
tool authority.

Regression coverage in `src/judgment/completion.rs` and
`src/eval/judgment/completion/tests.rs` covers input bounds, exact text,
host ceilings, terminal refusal, shared budgets, privacy, ownership, dropped
streams, the public cell deadline, both kernels and model-facing discovery.
These Rust regressions were authored, not compiled or run in the implementation
environment. `dsr quality --tool pi_agent_rust` remains the acceptance command.

## Schema-validated completion data

`CompletionRequest::parse` and `__pi_complete` also accept an optional `schema`
containing a JSON Schema object or boolean. The host compiles it before reserving
an attempt, resolving credentials or sending a model request. An omitted
`$schema` uses Draft 2020-12; an explicit declaration must be recognized by the
existing `jsonschema` engine. Local definitions and references work. A dedicated
retriever refuses external resources, including HTTP and local files, even if
another dependency enables those retrieval features. No schema grants I/O.

With a schema, the provider receives a screened JSON envelope containing the
prompt and schema, plus a fixed JSON-output instruction. This is host-validated
JSON generation, not a claim that every provider offers constrained decoding.
The prompt, system, schema descriptions, examples and constants all pass through
the same invocation privacy projection. If that projection changes the schema
or damages the envelope, the call fails before provider admission. It never
sends the unscreened original or validates against a weakened contract.

Only a clean terminal response containing exactly one JSON value is accepted.
Markdown fences, prose wrappers, extra trailing values and duplicate keys at any
object depth are errors; the host does not extract, repair or retry them. The
parsed value must pass the original compiled schema without coercion. A valid
response adds `data` and `schemaValid: true` to the existing `pi.completion.v1`
reply, retaining the exact terminal `text`, model identity and reported usage.
JSON null is a valid `data` value when allowed by the schema. Without a schema,
`data` and `schemaValid` remain absent and text behavior is unchanged. Failure
never yields an invented null or a partial successful data result.

Schema input is limited to 16 KiB encoded, and the combined prompt/schema JSON
envelope to 128 KiB. The existing common JSON limits apply: depth at most 32
and at most 65,536 values. The original 64/16 KiB prompt/system limits and
32 KiB terminal-response limit remain in force. JSON escaping counts toward
encoded limits. Schema compilation and validation are bounded-input synchronous
library operations, not a preemptible sandbox or a promised CPU-time bound.
Formats and annotation keywords follow the selected engine draft's semantics;
a schema validates structure, not the factual truth of a model's output.

Malformed contracts report `PI_COMPLETION_SCHEMA`; malformed or nonconforming
terminal JSON reports `PI_COMPLETION_RESPONSE`. Diagnostics do not echo schema
values, prompt fragments or failed data. A rejected terminal result consumes
its original attempt and retains a failed host receipt with unknown usage,
consistent with other completion failures. There is no automatic schema repair,
extra attempt, provider fallback or extension of the cell deadline.

For example, supply this JavaScript cell to Eval with an explicit model:

```json
{
  "kernel": "js",
  "judge_model": {"provider": "your-registered-provider", "model": "your-model-id"},
  "code": "globalThis.extraction = JSON.parse(await tool.call('__pi_complete', {prompt: 'Extract the file paths from these findings: src/api.rs and src/ui.rs require review.', schema: {type: 'object', required: ['files'], additionalProperties: false, properties: {files: {type: 'array', items: {type: 'string'}}}}})); display(extraction.data.files);"
}
```

Python uses the same native validation and can consume the returned data:

```python
import json
extraction = json.loads(await tool.call('__pi_complete', {
    'prompt': 'Extract the file paths from these findings: src/api.rs requires review.',
    'schema': {
        'type': 'object', 'required': ['files'], 'additionalProperties': False,
        'properties': {'files': {'type': 'array', 'items': {'type': 'string'}}},
    },
}))
display(extraction['data']['files'])
```

The result can stay in the current kernel for later cells or become input to
ordinary native tools. Such tool calls still use the current cell's normal host
policy; schema validation is not approval of a path or authorization to act.
An extraction error can be caught without resetting an otherwise healthy
kernel. The failed model attempt remains in native receipts, and retrying is
always an explicit additional request under the remaining shared allowance.

Schema regressions are in `src/judgment/completion/schema.rs` and the existing
`src/eval/judgment/completion/tests.rs`. They cover strict data acceptance,
preflight, privacy integrity, native file-read composition and retained state
in both kernels, failed receipts and dropped streams. These added Rust tests
were authored, not compiled or executed in the implementation environment:
`dsr quality --tool pi_agent_rust` could not start because `dsr` was missing.

## Retained background model jobs

Agent-owned eval cells can explicitly start a completion, judgment, or judgment
batch and return before its provider work finishes. The native session owns
the task, its cancellation, retained result and completion notice. A later
cell can collect the result even if the original kernel was reset or used
`kernel_mode: "per_call"`.

The foreground operations above keep their existing behavior. These separate
start operations accept their ordinary request fields plus `timeout_secs`:

| Start operation | Request | Default task timeout |
|---|---|---|
| `__pi_model_complete_start` | `prompt`, optional `system`, `schema`, `max_tokens`, `images` | 60 seconds |
| `__pi_judge_start` | `state`, `questions`, optional `images` | 60 seconds |
| `__pi_judge_batch_start` | `states`, `questions`, optional `images`, `concurrency` | 300 seconds |

`timeout_secs` must be an integer from 1 through 600. Its budget starts at native
admission preparation and is not renewed when the task launches; each provider
request still has its existing 60-second ceiling. The launching cell's deadline
also covers preparation and admission. An accepted task can continue independently
of the kernel, while remaining subject to its original Agent cancellation
context and the session's job shutdown.

For example, pass this cell with an explicit `judge_model` or a host-bound Judge:

```javascript
globalThis.request = JSON.parse(await tool.call('__pi_model_complete_start', {
  prompt: 'Extract the paths from: src/api.rs and src/ui.rs need review.',
  schema: {type: 'array', items: {type: 'string'}},
  timeout_secs: 60
}));
display(request.jobId);
```

The acknowledgment has this shape:

```json
{
  "schema": "pi.eval.model-job.v1",
  "operation": "completion",
  "jobId": "job-id-returned-by-the-host",
  "timeoutSeconds": 60
}
```

Use the current cell's normal `jobs` tool access to collect it:

```javascript
const observation = await tool.result('jobs', {
  action: 'wait', jobId: request.jobId, timeoutMs: 1000
});
const job = observation.details;
if (job.status === 'exited') {
  display(job.result.details.result.data);
}
```

This observation wait does not extend or reset the task deadline. Use
`timeoutMs: 0` to poll and `action: 'cancel'` to cancel. A running snapshot has
no final result; a running batch can contain an explicit progress observation,
as described below. If the guest variable has been lost, `jobs {action: 'list'}`
shows the session's accepted jobs. Model jobs use the fixed labels
`eval model completion`, `eval judgment` and `eval judgment_batch`, and their IDs can
be used from either kernel. Repeating a start call creates another request;
there is no automatic replay after a lost acknowledgment.

The earlier raw `__pi_complete_start` operation keeps its
`pi.completion.background.v1` acknowledgment, ordinary `pi.completion.v1`
result details, fixed 60-second budget and `eval completion` label. Its jobs
remain accessible through ordinary `jobs` calls. The `completion()` helper
uses `__pi_model_complete_start` and attaches only to that operation's label;
see [the raw completion contract](eval-background-completions.md).

### Admission and privacy

Background starts require the current Agent invocation's tool host and owned
privacy-snapshot factory. A plain standalone `EvalTool::execute` or the older
borrowed `execute_with_host` interface cannot manufacture this authority.
Admission checks access to the current session's `jobs` tool. This check and
image reads consume the ordinary 64-call host-tool allowance.

All request shapes, schemas, image descriptors, image reads and aggregate
image budgets are checked in the launching cell. The snapshot is captured
after attachment reads. Complete text input is scanned together before
preparing individual requests, so a secret assignment in a later batch item
also protects earlier echoes and the eventual retained output. Changed rubric
identities, batch keys or schema contracts refuse the whole admission before
any provider request starts. Binary image evidence remains outside text
privacy screening, as described above.

Only the explicitly bound Judge, screened owned requests, fixed privacy
snapshot and cancellation context enter the task. No borrowed Agent, live
privacy callback, interpreter state, image reader or tool dispatcher is
retained. The model selection is frozen for the accepted task; later cells do
not inherit it for new requests. There is no role selection, retry, model
fallback, history, or tool execution within the provider requests.

Foreground attempts and background item reservations share the **16-attempt
allowance per launching cell**. A whole batch reserves all its items before
awaiting admission. Existing batch limits still apply: 1–16 states, concurrency
1–8, 256 KiB expanded text and 16 MiB encoded images. Each background operation
uses one ordinary native job slot. The process-wide limit of eight live jobs
is shared with shell jobs, subagents and workpools; a capacity or shutdown
rejection starts no provider work.

All auxiliary provider requests also share a **process-wide 32-request gate**.
This covers foreground completions and judgments, every batch item, and retained
model jobs because they converge on the same terminal request collector. Waiting
for a slot stays inside the request's existing 60-second ceiling and inherited
cancellation; it does not grant a fresh deadline, retry a request, or reserve a
provider call before a slot exists. A permit is released when stream collection
finishes, fails, times out, or is cancelled.

### Results, errors and retention

The ordinary `pi.host_job.v1` snapshot contains the final native `ToolOutput`
in `result`. A completed model task's `ToolOutput.details` has schema
`pi.eval.model-job.result.v1`, its `operation`, its model `result`, and native
attempt receipts under `judgments`. The nested result uses the same
`pi.completion.v1`, `pi.judgment.v1`, or `pi.eval.judgment-batch.v1` schema as the
foreground operation. Content text gives a short summary; complete structured
results and usage are in details.

A completion or single judgment failure has `result: null`, a sanitized error,
and `isError: true`. A fully finished batch retains every item in input order,
including per-item failures. If any item failed, the native job is failed and
its output has `isError: true`, while the complete batch remains available for
inspection. A cancelled or timed-out task remains an error. For batches, the
native job preserves its last accepted progress observation alongside that
error; it does not fabricate a complete batch or replay its unfinished items.
Generic task failures, including privacy refusal and panic, discard progress.

### Batch progress and interrupted results

Batch handles expose `progress()` in JavaScript and Python. It returns the
latest native observation without consuming results or starting more work:

```javascript
globalThis.review = await judgeBatch(
  ['The first claim and its evidence', 'The second claim and its evidence'],
  {supported: {type: 'bool', instructions: 'Is this claim supported?'}},
  {concurrency: 2, timeoutSeconds: 120}
);

// In this or a later cell, including after reconstructing the handle with attach:
const progress = await review.progress();
if (progress) {
  display(progress.items.filter(item => item.status === 'completed'));
  display({completed: progress.completed, queued: progress.queued, running: progress.running});
}
```

`progress()` returns `null` in JavaScript or `None` in Python until an
observation is available, and when a terminal failure retained no typed evidence.

An unfinished observation has schema `pi.eval.model-job.progress.v1`, operation
`judgment_batch`, and `complete: false`. It includes every original item in
input order, the native attempt receipts, and the following counts:

- `total`: all reserved items.
- `completed`: finished items, equal to `succeeded + failed`.
- `succeeded` and `failed`: completed verdicts and per-item errors.
- `queued` and `running`: pending items, so `total = completed + queued + running`.

Each row retains its original typed `key`. Its `status` determines which
evidence is present:

| Status | Evidence |
|---|---|
| `queued` | No verdict or error; the request has not entered a provider wave. |
| `running` | No verdict or error; the request belongs to the current bounded wave. |
| `completed` | A native typed `verdict`; no error. |
| `failed` | A sanitized `error`; no verdict. |

Running counts describe admitted work in the current wave; they do not prove
that a provider received a request or billed it. Pending receipts report
unknown usage. Progress is published before the first provider wave, when a
wave is admitted, and after each result before another queued request can
start. Each update is a complete screened snapshot. Reading it repeatedly
does not drain, duplicate, or replay completed items.

Through the raw `jobs` API, a running job's `result.details` has schema
`pi.host_job.progress.v1` and a `partialResult` containing the native progress
`ToolOutput`. The progress object is at `result.details.partialResult.details`.
After cancellation or timeout, the job status remains `killed` or `timedOut`,
the outer result has `isError: true`, and its details use
`pi.host_job.interrupted.v1` with the same `partialResult` shape. The inner
observation keeps `complete: false`; its queued/running counts record the last
accepted observation, not work continuing after settlement. Only progress
accepted before the cancellation fence can be retained.

The handle's `progress()` also exposes the observed `jobStatus`. For a fully
finished batch it derives a `complete: true` view from the ordinary final
aggregate, without changing that final result's schema. `wait()` keeps its
normal completion/error behavior; it does not return partial evidence as a
successful batch. Use `progress()` or `output()` to inspect interrupted work.

Privacy screens the complete aggregate, including every finished result,
pending key, receipt, count and schema identity, before each publication.
A change to typed evidence or identity refuses the new observation and
withdraws previously retained progress before terminal publication. The same
rule applies when screening the final aggregate. A racing cancellation cannot
preserve an older snapshot that this failure has invalidated.

Only the latest accepted progress snapshot is retained in memory. Progress
updates create neither additional artifacts nor completion notices; terminal
publication writes the final or interrupted result through the ordinary jobs
artifact policy.

Launch receipts include the accepted job ID. Final successful-attempt receipts
contain reported usage; failed attempts retain unknown usage. They do not
update the main Agent's cost counters or provide a durable billing ledger.
One completion notice is delivered through the existing session job mechanism.
Polling a result does not consume that notice.

Retention follows native jobs: process-local state, at most 128 settled jobs
per session and 512 process-wide, with a shared 32 MiB retained native-result
budget covering live progress and settled results, and a 1 MiB result ceiling.
Older settled entries may be pruned as
these limits are reached. Kernel reset preserves accepted jobs; restarting
the Pi process does not resume them. The jobs artifact policy remains in
effect, and the same captured privacy projection screens retained results,
artifacts and completion notices.

Native regression fixtures in `src/eval/judgment/background/tests.rs` exercise
controlled provider streams and real job ownership, including ordered mixed
progress, retained cancellation/timeout evidence, and withdrawal after either
progress or final-result privacy refusal. The prepared request and
privacy tests also cover callback lifetime, schema integrity and cross-item
secret discovery. These fixtures require the authoritative
`dsr quality --tool pi_agent_rust` run; source review is not native execution
evidence.
