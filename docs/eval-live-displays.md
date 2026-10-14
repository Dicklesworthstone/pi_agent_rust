# Live rich eval displays

Python and JavaScript `display()` deliver JSON, plain text, Markdown and PNG snapshots
through the existing `ToolUpdate` callback while the cell is still running.
For example, a long analysis can display its first plot before computing the
next one. `%load` and awaited async cells use the same path. No extra
input flag, tool permission or frontend-specific callback API is required.
Both kernels feed the same Rust host collector and MIME validator.

```python
display({"phase": "loaded", "rows": 1200})
# Continue the computation; the first snapshot can already be visible.
display("## Analysis in progress", mime="text/markdown")
```

In the retained JavaScript kernel, MIME selection remains the second argument:

```javascript
display({phase: "loaded", rows: 1200});
await sleep(10);
display("## Analysis in progress", "text/markdown");
```

JavaScript strings still default to JSON; use `"text/plain"` for literal text.
PNG inputs remain `Uint8Array` or standard base64, never a file path or URL.
Loaded scripts and imported JavaScript/TypeScript modules share their cell's
sink; a module can emit progress before awaiting a native tool read. Module
caching and explicit reset behavior are unchanged. Captured values are not
passed between runtimes: native frames contain detached JSON only.

## Update and final-result contract

Updates replace the previous preview; they are not append-only deltas. They
retain `status: "running"`, native image content blocks, `details.displays`
MIME descriptors and `details.jsonOutputs` for included JSON values. Console
previews precede rich displays, whose own call order is retained. This is not
a combined stdout/display timestamp log, and consumers must not append every
replacement update to the transcript or mistake an update for cell success.

`details.displayStream` uses schema `pi.eval.display_stream.v1`. Its sequence
counts admitted rich displays, separately from console frames. `bytes` counts
the serialized MIME frames. Running previews report `complete: false` and
`preview: true`, even when the guest has already emitted its last display.
The final result reports `complete: true` only after the returned collection
matches all received snapshots and passes the ordinary MIME formatter. This
means the display collection is complete, **not** that execution succeeded:
an ordinary code exception still returns an errored tool result.

Snapshots are detached when `display()` is called. Later mutations cannot
change them. Once any live frame is received, a missing, reordered, appended
or rewritten final collection is a protocol error, and the kernel is retired.
The final formatter emits the full collection once, not another copy of each
live update. Raw/final-only kernel consumers retain their existing behavior.

## Bounds and failures

The existing shared MIME validator admits each live frame before callback
delivery, with the same 32-display, 2 MiB item, 4 MiB aggregate, JSON structure
and PNG envelope/dimension bounds as final output. It does not decode PNG
pixels, read paths, fetch URLs or execute notebook serialization hooks.
A refused frame changes no accepted prefix. The complete final collection
is validated again; earlier provisional updates cannot be retracted if a
later frame is invalid, the kernel crashes or the request is cancelled.

Replacement rich previews have a separate 256 KiB MIME payload budget. JSON
is charged twice because it appears as both text and structured data; image
and text frames are charged once. Bounded descriptors and omission notices
are additional. An item that cannot fit is omitted whole, never cut into
invalid JSON or a partial PNG. Its descriptor reports `previewOmitted: true`;
`displayStream` reports `previewBytes`, `previewLimit` and `omittedDisplays`.
Live `jsonOutputs` contains only included values. Successful completion still
returns every full admitted value under the ordinary 4 MiB aggregate budget.
Omission is not artifact persistence: a cancelled large display may have no
complete model-visible value.

Existing cell ownership checks, cancellation, deadlines, bounded transport
queues and coalescing remain in force. A rich-only update can trigger the same
host cancellation/session-change handling as stdout. Python's transport-stop
contract remains a textual tool Error: it reports `EVAL_DISPLAY_INCOMPLETE`
and the count of received provisional displays rather than embedding PNG
bytes in error text. Final ordinary Python exceptions still retain their
full validated displays. These are cooperative boundaries, not a sandbox,
rollback, managed background execution or a general guest-memory bound.

### JavaScript delivery and retirement

QuickJS uses the same per-invocation bounded channel as native console output,
but a separate rich-display sequence. The existing host drains and coalesces
both streams; it still validates a complete MIME frame before calling the
user's update handler. No new queue, execution thread, or callback interface is
created. The dedicated kernel thread waits on channel backpressure while
observing its deadline and revocation flag. Slow callbacks can therefore slow
the cell and use its existing timeout budget.

Guest serialization and proxy traps finish before the native capture is
borrowed. That borrow is released before delivery waits or exception creation;
a reentrant display/console call cannot borrow a partially mutated sink.
Both native senders close before the final reply so the host can drain the
complete accepted stream before comparing final snapshots.

A disconnected or expired rich-output delivery is latched by the native sink.
Even a guest `try/catch` cannot turn it into successful execution or keep the
realm reusable: the worker reports failure and resets its state. Effects that
already happened are not rolled back, and catching an exception does not
guarantee that no subsequent guest statement ran before retirement. Ordinary
data-validation errors remain catchable without poisoning an otherwise healthy
cell. Invalid live PNG/MIME data is refused by the host and retires that kernel.

The existing JavaScript watchdog result retains bounded provisional rich
content with `complete: false` when there is no validated final collection.
A validated final collection accompanying a code exception can be complete
while the tool result is still an error. Always inspect `is_error`/`isError`
and state-reset diagnostics, not just `displayStream.complete`.

`output_path` remains a **console-only** JSONL archive. Rich frames neither
enter that file nor change its schema, byte totals or console sequence. An
interrupted run still does not publish a misleading complete archive. This
feature does not add rich-output artifact spill or durable notebook storage.

Regression coverage is in `src/eval/output/live/tests.rs`,
`src/eval/js_kernel/display.rs`, `src/eval/js_kernel/console.rs`, and
`tests/eval_live_display.rs`, including callbacks that must unlock a cell's
next tool read before that cell can finish. Run the authoritative
`dsr quality --tool pi_agent_rust` workflow for Rust acceptance. Python-server
probes alone do not establish Rust integration or frontend rendering support.
The JavaScript additions include real kernel transport and public `EvalTool`
regressions; source inspection does not establish that those tests pass.
