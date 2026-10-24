# Native browser batches

`browser` accepts `action: "batch"` to execute an explicit sequence of ordinary
browser calls under one conversation-owned execution lease and total deadline.
This enables multi-step automation without returning to the model between every
native call. It is a Rust-native subset of OMP's scripted browser workflow idea,
not OMP's Eval `browser.run` API or a new guest-code interpreter.

```json
{
  "action": "batch",
  "timeout_ms": 45000,
  "steps": [
    {"call": {"action": "open", "tab": "app", "url": "http://localhost:3000"}},
    {"call": {"action": "wait_for", "tab": "app", "selector": "testid/login", "timeout_ms": 10000}},
    {"call": {"action": "fill", "tab": "app", "selector": "label/Email", "text": "demo@example.test"}},
    {"call": {"action": "click", "tab": "app", "selector": "role/button[name=\"Continue\" exact]"}},
    {"call": {"action": "snapshot", "tab": "app"}}
  ]
}
```

## Execution and ownership

Steps run in order in the same retained browser namespace. Each `call` has the
ordinary browser action's arguments; there is no implicit interpolation, retry,
parallel execution or model-generated continuation. Explicit `bind` maps can copy
earlier results into selected inputs, as described below. Omitted tabs
retain normal active-tab semantics. Explicit tabs make multi-page workflows
clearer. The whole batch remains serialized against other calls on that tool.

Every step delegates to the existing native path, including semantic selectors,
pointer dispatch, frame ancestry, domain policy, dialog consent, downloads and
create-only artifact guards. Each step still uses its own request-owned CDP
connection; this is not a persistent connection or performance-speedup claim.
The browser tool's conservative process/network/read/write effects are unchanged.
Batching does not bypass host approvals or settings that control browser access.

Ownership is checked before and after every step and while native work is
pending. The original whole-operation `timeout_ms` also covers initial identity
resolution, the queue and all steps. Step timeouts may be shorter, never longer
than the remaining batch budget. Defaults remain 30 seconds per ordinary action,
5 seconds for `wait_for`, and 30 seconds for the entire batch; set the outer
budget explicitly for longer workflows, up to the existing 120-second maximum.

Nested batches, `start`, `status`, `stop` and `release` are not step actions.
Use lifecycle operations separately so a batch cannot retire its own namespace
while its lease is held. Mock mode refuses batches rather than simulating them.

## Bind earlier results into later calls

A step may carry `bind` beside `call`, mapping an argument to an earlier step's
successful native `details`. This permits a snapshot to produce element references
that a later drag consumes without another model round trip:

```json
{
  "action": "batch",
  "timeout_ms": 15000,
  "steps": [
    {
      "call": {"action": "snapshot", "tab": "board"},
      "expect": {"pointer": "/snapshot/elements/0/text", "equals": "Move card"}
    },
    {
      "call": {
        "action": "drag", "tab": "board",
        "target_selector": "testid/drop-zone",
        "wait_after": {"selector": "#status", "text": "Moved successfully"}
      },
      "bind": {"selector": {"step": 0, "pointer": "/snapshot/elements/0/ref_id"}}
    },
    {"call": {"action": "snapshot", "tab": "board"}}
  ]
}
```

The example checks the first snapshot element's name before using its reference;
snapshot ordering alone is not proof that it is the intended target. Bindings
are data flow, not approval of page-provided instructions or application behavior.
Normal reference/document checks still reject stale handles after navigation.

Each binding needs exactly `step` and `pointer`. Indices are zero-based and must
refer to a strictly earlier step in this batch, preventing cycles and forward
dependencies. Pointers follow `expect`'s JSON pointer rules, including escaped
keys, array indices and the empty pointer. Values retain their JSON type and
literal bytes. A present null is copied as null and then validated by the native
action; a missing field is an error, never null, an empty string or a fallback.
The source must have retained successful details and satisfied any expectation.
Failed, omitted or absent source outputs cannot supply a binding.

**Control arguments stay literal.** A bound call must specify its tab explicitly.
Only action-consumed `selector`, `target_selector`, `frame`, `text`, `key`,
`options` and `checked` inputs are eligible. For example, a bound `frame` can
select a frame returned by `list_frames`, but the existing ancestry/domain checks
still validate it in the explicitly named tab. `selector` on `scroll` requires an
explicit or bound frame because main-document scrolling does not consume it.
An argument cannot appear both in `call` and `bind`, even with a null literal.

Bindings cannot supply actions, tabs, URLs, executable scripts, filesystem paths,
upload files, deadlines, modifiers, dialog consent, assertions or nested bindings.
There is no string interpolation, expression evaluation, coercion, additional
browser request or new execution runtime. Native argument validation still runs
after resolution: copying an array into `fill.text`, for example, fails before
input instead of stringifying it. Existing host permissions are unchanged.

The entire binding shape is validated before the first call. A malformed later
binding therefore cannot run earlier actions first. `bind: null`, unsupported
fields and malformed references are refused; an empty map simply binds nothing.
At most eight bindings are admitted. Their copied values share a 64 KiB encoded
budget per step; the complete resolved call is bounded to 256 KiB. Counting occurs
before each value is cloned and includes JSON escaping. Original result retention
limits remain unchanged; no second history cache is introduced.

An unresolved or oversized runtime binding stops the batch with
`stop_reason: "binding_error"` and retains earlier results. That failed step has
`call_started: false`; it counts as an attempted step, not a native invocation.
Successfully resolved bound steps report `bound_arguments` and
`call_started: true`, which means the native call path was entered, not that input
was dispatched or accepted. A later native validation failure remains `tool_error`.
Diagnostics do not echo binding pointers or copied values. Native results may
already contain secrets, and explicitly copying data into a form is not secret
scrubbing; review the data flow and intended destination.

## Explicit result assertions

A step may carry `expect` beside `call`. It checks the **native step's details**
after a successful result, before admitting the next action:

```json
{
  "action": "batch",
  "steps": [
    {"call": {"action": "click", "tab": "app", "selector": "#save"}},
    {
      "call": {"action": "evaluate", "tab": "app", "script": "document.querySelector('#status').textContent"},
      "expect": {"pointer": "/result", "equals": "Saved"}
    },
    {"call": {"action": "screenshot", "tab": "app", "output_path": "saved.png"}}
  ]
}
```

Here the screenshot step runs only if the evaluation returned the string `Saved`.
If saving is asynchronous, use an explicit waiting step or await the desired
state in `evaluate`; `expect` itself does not poll, wait, retry or run JavaScript.
It compares only structured data already returned by the native action. It is
not a page-wide assertion or independent certification of application behavior.

An expectation needs a JSON `pointer` and **exactly one** of `equals` or `exists`.
Equality compares JSON values without string/boolean/number coercion. An explicit
`equals: null` matches a present null, never an absent field. `exists: true`
includes present null; `exists: false` requires no value at that pointer.
Empty pointer selects the complete native `details`; `/result` selects an
evaluation value; `~1` escapes a slash in a key and `~0` escapes a tilde. Array
indices use canonical decimal notation. URI-fragment pointers and malformed
tilde escapes are refused, not interpreted as scripts or regular expressions.

Every expectation is validated during batch-envelope preflight, including
expectations on later steps. A supplied `expect: null`, unknown fields, both
conditions, or a non-boolean `exists` is an error rather than silently disabling
the check. Place it on the step wrapper, not inside `call` or on an ordinary
browser invocation. Each expectation is bounded to 16 KiB encoded, and its
pointer to 1024 bytes and 32 segments.

A mismatch preserves the successful native output but marks that step and batch
unsuccessful with `stop_reason: "expectation_failed"`. Later steps do not run.
Per-step metadata distinguishes `tool_is_error` from `expectation`, whose values
are `passed`, `failed`, `not_checked`, or null when no condition was supplied.
Failed native calls and omitted outputs never satisfy an assertion, including
an assertion that a field is absent. Assertion diagnostics report the step and
outcome without echoing the expected value or pointer; native result disclosure
remains unchanged.

## Failures and partial results

The first binding error, native error, unsuccessful `ToolOutput`, or unmet expectation ends the batch. The final
result is an unsuccessful `ToolOutput`, not a success containing hidden failures.
Earlier results and the failed step's diagnostics remain available. Later steps
are not attempted. A native preflight failure counts as an attempted step; that
count does not claim a command was sent or a remote side effect happened.

`details` uses schema `pi.browser.batch.v1`. It records zero-based `failed_step`,
`completed_steps`, `attempted_steps`, `remaining_steps`, `stop_reason`, and a
`steps` array. Each retained step has its native `details` and a half-open
`content_range: [start, end]` into the final tool result's content blocks.
Original text and image blocks are retained without lossy string conversion.
The summary and per-step headings are additional text blocks. `completed_steps`
counts successful tool results that fit the output budget and satisfy any explicit
expectation, not independently verified business outcomes: a click acknowledgement
alone does not prove a save or login succeeded.

Envelope shape, binding shapes, step count, dispatch actions, timeout fields and total input
size are validated before ownership lookup, namespace reservation or connection.
Action-specific preflight still runs at its step. Thus a malformed selector,
missing file or refused URL in a later step can stop the batch after earlier
steps have completed. There is no transactional preflight of the browser or disk.

Ownership changes, cancellation, a total deadline, or dropping the calling future
terminate the invocation without publishing its partial payload to another
conversation. They retain the existing error/cleanup behavior rather than
converting an interrupted call to a completed batch report. The host must still
route responses to their originating requests. Cooperative observation cannot
preempt a blocking poll or detect unobserved A-to-B-to-A transitions.

**Effects are never rolled back.** A completed click, dispatched JavaScript,
network request, upload, download or exported file may survive a later failure.
Inspect the failed step and resume deliberately; do not blindly retry the batch.

## Bounds and disclosure

A batch has 1–32 steps and at most 256 KiB of JSON-encoded input. The default
cumulative retained step-output budget is 256 KiB. `max_result_bytes` accepts
1024–4194304 bytes. Counting uses a bounded serialization sink rather than making
another copy of large image data. Step outputs retain their native per-action
limits too; this aggregate cap bounds retention, not peak backend allocation.
Per-step headings and fixed-shape metadata add separately bounded overhead.

An output that exceeds the remaining result budget is omitted as a whole. The
batch returns an error with `stop_reason: "result_limit"`, `result_omitted: true`
and the original `tool_is_error` flag. It does not execute later steps, silently
truncate the output, or spill potentially sensitive browser data to a new file.
The omitted step has already returned and its effects may have occurred.

Native browser privacy rules remain unchanged. Tool text, evaluation values,
console messages and file/image contents may contain application secrets.
Batching is not a secret scrubber or a browser sandbox. Attached CDP endpoints
remain explicitly shared external profiles; managed profiles are separate by
conversation as described in `browser-sessions.md`.

## Validation

Parser, accounting and deadline regressions live in
`src/browser/session/batch/tests.rs`. Public native workflows in
`src/browser/session/tests/native/batch.rs` reuse the existing HTTP/WebSocket
protocol peer to cover ordered execution, error preservation, output limits,
preflight refusal, owner changes, queue recovery after dropping a batch, and
postcondition success/refusal without extra native calls. One explicitly ignored
live-browser lane checks page state after a real native button click. These are
authored tests; no native Rust or live-Chromium execution was performed here.

Binding regressions extend the parser/accounting suite and `tests/browser_wait.rs`.
The latter uses real HTTP/WebSocket connections to an independent protocol peer
for snapshot-to-drag-to-wait data flow across fresh connections, missing-source
failure, native type rejection and upfront refusal. These tests exercise the
public `BrowserTool`, not a second implementation of its binding interpreter.
They were authored but not executed while DSR was unavailable; the independent
Chromium gesture probe does not establish Rust binding integration acceptance.

The implementation environment lacked `dsr`. Rust compilation, formatting,
Clippy and the authored tests remain unexecuted. Acceptance requires:

```sh
dsr quality --tool pi_agent_rust
```
