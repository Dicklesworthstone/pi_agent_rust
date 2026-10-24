# OMP eval input compatibility and native image output

The native `eval` tool accepts OMP-style `language` and `timeout` fields as
aliases for Pi's existing `kernel` and `timeout_secs` fields. Existing native
calls are unchanged. OMP's current single-cell language tokens are `py` and
`js`; Pi additionally accepts `python` and `javascript`.

```json
{"language":"js","code":"globalThis.answer = 42; answer","timeout":10}
```

A later native call shares the same language/session namespace:

```json
{"kernel":"js","code":"answer"}
```

A requested language is never ignored in favor of Python. Unknown values,
including `auto`, fail before admission or reset. Supplying both aliases for
one field (even with matching values) is an error, rather than an implicit
precedence rule. A rejected reset request does not discard either kernel.

Timeout values retain Pi's native policy: nonnegative integer seconds,
default 30, clamped to 1–600. In particular, `timeout: 0` does **not** disable
the watchdog as it does in OMP. Fractional seconds are not accepted. These
aliases do not add backend sniffing or multiple cells. Agent-owned eval now
supports [policy-checked tool orchestration](eval-structured-results.md#agent-scoped-tool-orchestration)
through `tool.call` and `tool.result`, plus the explicit typed-judgment bridge.
The [retained `agent()` and `wait()` helpers](eval-structured-results.md#retained-background-agent-jobs)
launch and manage native background subagent jobs across cells. Set
`keepAlive: true` / `keep_alive=True` for a [reusable agent](eval-structured-results.md#reusable-agents-and-queued-turns)
with queued `send()` follow-ups in the same live child conversation.
[Reusable `workpool()` helpers](eval-structured-results.md#reusable-workpools)
distribute up to 32 items across 1–8 native workers, retain per-turn handles,
and support inspection and explicit cleanup across cells. `finish()` seals
admission, drains accepted work, reaps the children and completes one aggregate
session job.

[Retained model helpers](eval-structured-results.md#retained-background-model-requests)
provide `completion()`, `judge()` and `judgeBatch()` / `judge_batch()`, with
later-cell waits, cancellation and attachment by job ID after a reset. They use
the launching cell's explicit native `judge_model` binding. Native
[per-request model roles](eval-structured-results.md#per-request-model-roles)
allow `judge_model: {role: 'smol'}` and helper `model: 'slow'` / `model='slow'`
to route requests to different configured models in one cell. Roles resolve
from trusted `modelRoles` settings and the host catalog, with configured
thinking levels and disabled-provider filtering. Unlike ordinary conversation
role resolution, eval requires each requested role to be configured explicitly;
it does not inherit CLI flags or fall back to another model. Host-bound SDK
Judges cannot be overridden. Completions and
judgments accept image evidence through the current host's read policy, and
completion images can be combined with a validated output schema. Batch
`progress()` retains ordered per-item states and finished verdicts across cells,
including the last valid progress after cancellation or task timeout. JavaScript
batch `drain({cursor, timeout})` and Python `drain(cursor=..., timeout=...)`
expose [native cursor-based result pages](eval-background-completions.md#collect-batch-results-as-they-finish).
Callers acknowledge pages explicitly after processing; both helpers preserve
partial failures and current host access checks without implicit cursor state
or guest polling. The raw `__pi_judge_batch_drain` bridge remains available.
Retained `judgeBatch` / `judge_batch` accepts `retries` (0–3) and `minOk` /
`min_ok` (0 through item count). Defaults stay at zero retries and all items
required, intentionally preserving native behavior rather than OMP v18.12.6's
defaults of one retry and one required success. The full worst-case attempt
budget is reserved before admission; retries retain the original deadline,
never rerun successful peers, and only retry malformed answers or classified
transient provider failures. `meetsMinimum` reports the threshold separately
from truthful `allOk`, with all settled items and per-attempt receipts retained.
Automatic backgrounding of arbitrary eval code remains separate work; unknown helper
options fail explicitly.
The typed `completion()` helper uses the uniform model-job bridge
`__pi_model_complete_start`. The existing raw `__pi_complete_start` contract
remains available through `tool.call`, with its own acknowledgment and direct
completion result details; see [raw completion jobs](eval-background-completions.md).
Both use ordinary native session jobs rather than separate schedulers.

Pi's default `agent()` remains one-shot; warm reuse is explicit. The bounded
pool assigns new items to idle children by observed context load, using the
post-compaction idle barrier's token estimate and model capacity. Busy children
keep balanced FIFO queues. JavaScript `pool.drain({cursor, timeout, maxItems})` and
Python `pool.drain(cursor=..., timeout=..., max_items=...)` collect up to four
completed, failed or cancelled full results per native call, across cells and
without waiting for earlier unfinished items. Explicit replayable cursors
acknowledge only processed item IDs; reads do not seal or cancel the pool.
See [incremental pool results](eval-structured-results.md#collect-completed-pool-items-across-cells).
OMP's keyed yield batches remain separate work: native pools execute one
prompt per item and use explicit `finish()` for aggregate completion. Pi's
`pool.close()` cancels running and queued work; worker lifetimes remain fixed
from their launch. See the linked contracts for supported isolation, schemas,
partial admission and cleanup behavior.

## JavaScript images

JavaScript `display(value, mimeType)` accepts `image/png`, `image/jpeg`,
`image/gif`, and `image/webp`. Pass a `Uint8Array` (including a subarray view)
or canonical standard base64. For example, with image bytes already obtained
inside the cell:

```javascript
display(jpegBytes, "image/jpeg");
display(webpBase64, "image/webp");
```

The model receives a native image content block, not a printed base64 string.
The SDK display descriptor includes width, height, MIME type and encoded frame
bytes, without another copy of the image. Inputs are explicit data: this helper
does not read paths, fetch URLs, execute SVG/HTML, or invoke custom serializers.
Python's public display helper remains PNG-only; this change extends the native
JavaScript helper and the shared host MIME validator.

Host checks preserve the existing limits: 32 displays per cell, 2 MiB per
serialized frame, 4 MiB across frames, 16,384 pixels per axis, and 16,777,216
pixels per canvas. JPEG baseline/progressive frame headers, GIF87a/GIF89a
logical screens, and WebP VP8/VP8L/VP8X headers are inspected without raster
decoding. MIME/header mismatches, invalid base64, missing envelope markers,
truncated segment/chunk boundaries and oversized canvases fail explicitly.
A bad image rejects the final rich-output collection rather than publishing a
partial successful result. Already-delivered live frames cannot be recalled.
These are envelope checks, not certification of image validity or bounds on
animation frame counts or downstream decoder memory.

## Regression coverage

`src/eval/lifecycle.rs` covers OMP aliases, ambiguous/malformed requests,
namespace reuse, explicit reset and watchdog behavior. Host image tests live
in `src/eval/display/images.rs`. They require the repository's Rust toolchain
and dependency workspace.

The embedded JavaScript encoder also has a dependency-free Node harness:

```sh
node --test tests/eval_image_display.test.cjs
```

This executes the actual factory, including all four MIME types, byte views,
base64 remainders, frame limits, unsupported types, snapshot semantics and
captured-intrinsic tampering. It is not a substitute for native QuickJS and
Rust integration tests.
