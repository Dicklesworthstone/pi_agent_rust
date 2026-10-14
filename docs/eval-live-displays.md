# Live rich eval displays

Python `display()` now delivers JSON, plain text, Markdown and PNG snapshots
through the existing `ToolUpdate` callback while the cell is still running.
For example, a long analysis can display its first plot before computing the
next one. `%load` and awaited async Python cells use the same path. No extra
input flag, tool permission or frontend-specific callback API is required.
The shared Rust host accepts the same protocol from either kernel; QuickJS
producer wiring is a separate change.

```python
display({"phase": "loaded", "rows": 1200})
# Continue the computation; the first snapshot can already be visible.
display("## Analysis in progress", mime="text/markdown")
```

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

`output_path` remains a **console-only** JSONL archive. Rich frames neither
enter that file nor change its schema, byte totals or console sequence. An
interrupted run still does not publish a misleading complete archive. This
feature does not add rich-output artifact spill or durable notebook storage.

Regression coverage is in `src/eval/output/live/tests.rs` and
`tests/eval_live_display.rs`, including callbacks that must unlock a cell's
next tool read before that cell can finish. Run the authoritative
`dsr quality --tool pi_agent_rust` workflow for Rust acceptance. Python-server
probes alone do not establish Rust integration or frontend rendering support.
