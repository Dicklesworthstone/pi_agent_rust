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
Both kernels also expose [explicit `tool.batch` groups](eval-structured-results.md#bounded-tool-batches)
of 1–16 native calls with bounded compatible concurrency and ordered mutation
barriers. Ordered items preserve full native results or explicit item errors,
and share the cell's 64-call allowance with scalar calls. Existing scalar
`Promise.all` and `asyncio.gather` dispatch remains serialized.
The [retained `agent()` and `wait()` helpers](eval-structured-results.md#retained-background-agent-jobs)
launch and manage native background subagent jobs across cells. Set
`keepAlive: true` / `keep_alive=True` for a [reusable agent](eval-structured-results.md#reusable-agents-and-queued-turns)
with queued `send()` follow-ups in the same live child conversation.
[Reusable `workpool()` helpers](eval-structured-results.md#reusable-workpools)
distribute up to 32 items across 1–8 native workers, retain per-turn handles,
and support inspection and explicit cleanup across cells. `finish()` seals
admission, drains accepted work, reaps the children and completes one aggregate
session job. Optional `maxBatchSize` / `max_batch_size` (1–8, default 1) freezes
how many queued assignments can share one child prompt. Larger values use
keyed final results while preserving each item's identity, status and schema
validation. Groups are also bounded to 64 KiB of task text and do not wait to
fill; the setting survives native recovery and cannot change after launch.
Grouped shared-workspace pools may also opt into `yieldResults: true` /
`yield_results=True`: the child receives a native `yield` tool that publishes
individual terminal results while it continues the remaining assignments.
Accepted results survive later child failure; physical reuse still waits for
the normal RPC idle barrier. Worktree pools keep final-only acceptance.

[Streaming map/reduce](eval-workpool-pipelines.md) is available as
`workpool.mapReduce()` in JavaScript and `workpool.map_reduce()` in Python.
It consumes sync/async iterables with bounded admission, reduces full native
results as they settle, and exposes per-result recovery cursors on failure.
Its admission window is `min(32, maxItems, concurrency * maxBatchSize)`, with
equivalent Python option names. It consumes early terminal yields when the
pool explicitly enables them, within the existing pool lifetime limits.
Background execution of an entire eval cell remains separate work.

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
it does not inherit CLI flags or substitute a missing or unavailable primary
binding. Host-bound SDK Judges cannot be overridden. Retained individual
completions and judgments support explicitly configured
[`retry.fallbackChains` routes](eval-structured-results.md#configured-model-fallback-and-attempt-inspection),
including nested exact-model chains, effort suffixes, error-class filtering
and asynchronous `attempts()` inspection in both kernels. All candidate
requests reserve the native 16-attempt cell budget up front and retain the
original privacy snapshot, session cost cap and task deadline. Native
`maxRetries` permits that many secondary requests after the primary; OMP
v18.16 uses it as the total request cap. Raw model operations and retained
batches retain fixed bindings. Completions and
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
Native grouped results default to a complete final keyed envelope and the
post-turn idle barrier before acceptance. Opt-in `yieldResults` permits
authenticated native tool receipts to settle individual items earlier; the
final envelope then covers only remaining keys. A malformed receipt or final
envelope fails unfinished work while preserving earlier accepted items;
valid per-key errors and schema failures preserve successful peers. Isolated items in a group share
one cumulative worker patch, identified by `details.workerBatch`; these are
not independent patches to stack. Pi uses explicit `finish()` for aggregate
completion, while `pool.close()` cancels running and queued work. Worker
lifetimes remain fixed from launch. See the linked contracts for supported
isolation, schemas, partial admission and cleanup behavior.

## Python and JavaScript images

JavaScript `display(value, mimeType)` accepts `image/png`, `image/jpeg`,
`image/gif`, and `image/webp`. Pass a `Uint8Array` (including a subarray view)
or canonical standard base64. For example, with image bytes already obtained
inside the cell:

```javascript
display(jpegBytes, "image/jpeg");
display(webpBase64, "image/webp");
```

Python accepts the same formats with `display(value, mime=...)`, using `bytes`,
`bytearray`, or canonical standard base64. Images returned by an enabled native
tool can be displayed directly, subject to the existing display size limits:

```python
result = await tool.result("read", {"path": "diagram.jpg"})
if not result["isError"]:
    for block in result["content"]:
        if block["type"] == "image":
            display(block["data"], mime=block["mimeType"])
```

Python's rich-object protocol also accepts `_repr_jpeg_()` alongside
`_repr_png_()`, and `_repr_mimebundle_()` can supply any of the four image types.
A rich final expression uses the same validation and snapshot path as explicit
`display`. Unsupported HTML/SVG representations are not requested or executed;
an invalid preferred representation raises an error instead of silently falling
back to text. Existing Pillow and Matplotlib adapters continue producing PNG.

The model receives a native image content block, not a printed base64 string.
The SDK display descriptor includes width, height, MIME type and encoded frame
bytes, without another copy of the image. Inputs are explicit data: this helper
does not read paths, fetch URLs, execute SVG/HTML, or invoke custom serializers.
Accepted image bytes retain their format without conversion or recompression.
Python checks image envelopes and encoded size before publishing a complete
frame; the shared host validator independently checks every received frame.

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

The Python subprocess harness exercises the bundled kernel's actual JSONL
protocol, including all formats, rich JPEG hooks, MIME bundles, malformed and
truncated envelopes, base64 canonicality, snapshots and byte/count/canvas limits:

```sh
python3 tests/eval_python_rich.py ImageDisplayTests RichProtocolTests
```

The public `EvalTool` integration regression is in `tests/eval_python_rich.rs`;
it verifies native image blocks, live updates, dimensions and kernel reuse after
an invalid image. The standalone Python harness does not replace the
authoritative DSR Rust quality lane.
