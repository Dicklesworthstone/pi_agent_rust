# Native browser batches

`browser` accepts `action: "batch"` to execute a literal sequence of ordinary
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
ordinary browser action's arguments; there is no implicit interpolation, variable
binding, retry, parallel execution or model-generated continuation. Omitted tabs
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

## Failures and partial results

The first native error or unsuccessful `ToolOutput` ends the batch. The final
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
counts successful tool results that fit the output budget, not verified business
outcomes: a click acknowledgement alone does not prove a save or login succeeded.

Envelope shape, step count, dispatch actions, timeout fields and total input
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
preflight refusal, owner changes, and queue recovery after dropping a batch.
They are authored tests, not evidence of actual Chromium behavior.

The implementation environment lacked `dsr`. Rust compilation, formatting,
Clippy and the authored tests remain unexecuted. Acceptance requires:

```sh
dsr quality --tool pi_agent_rust
```
