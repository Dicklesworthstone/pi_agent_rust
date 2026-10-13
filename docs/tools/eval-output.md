# Python eval streaming output

Python `eval` calls now deliver native `ToolUpdate` snapshots while the cell is
running. This uses the same callback consumed by interactive, RPC and SDK tool
execution, not a separate command or output polling tool. A snapshot replaces the
previous preview; consumers must not append each complete snapshot as a delta.

```json
{"kernel":"python","code":"import asyncio\nprint('Starting', flush=True)\nawait asyncio.sleep(1)\nprint('Finished')\n42"}
```

The host normally coalesces updates at 50 ms intervals and flushes pending output
before host-tool waits and final completion. Each preview keeps at most 32 KiB of
UTF-8 data from each of stdout and stderr, plus explicit omission notices. There
is no timer thread or orphan output task. Callback execution is synchronous on
the host's eval polling path; callbacks should return promptly. Coalescing is not
a hard real-time delivery guarantee.

## One capture, bounded storage

Python sends complete, sequenced output frames rather than retaining unbounded
`StringIO` objects. The host admits at most 64 KiB of decoded text in each stream
frame and verifies the cell ID and contiguous sequence. The final response must
confirm that sequence and may not contain a second copy of stdout or stderr.

Final results retain a bounded head and tail: at most 512 KiB of source UTF-8
bytes per stream, plus a labelled `EVAL_OUTPUT_TRUNCATED` notice in the omitted
middle. Small output is unchanged. Unicode scalars are never split or reordered.
This is sampled output, **not a persisted full-output artifact**. Omitted data is
not recoverable from this tool result. The execution can succeed even when its
console rendering is sampled; sampling is always disclosed separately.

`details.output` uses schema `pi.eval.output.v1` and records `sequence`,
`stdoutBytes`, `stderrBytes`, `stdoutOmittedBytes`, `stderrOmittedBytes`,
`truncated`, and `preview`. Omission counts describe that particular preview or
final rendering and exclude the notices themselves. Pure expression cells that
emit no console frames retain their previous details shape.

Stdout precedes stderr in the final text, followed by the existing rich displays
and final expression or exception. This preserves the established result layout;
it does not promise an interleaved stdout/stderr/display timestamp log. Rich
JSON, Markdown and PNG displays remain final-cell captures, independently
validated by the existing shared formatter. They are not duplicated in updates.

Final Python `repr` results above 1 MiB are refused with `EVAL_RESULT_LIMIT`, not
silently truncated. Oversized tracebacks keep an explicitly labelled bounded
tail. These rules limit retained/protocol output, not arbitrary Python memory,
custom `repr` work, or allocations performed by the cell itself.

## Cancellation, failure, and retained state

The per-call capture lives outside the active kernel future. Timeouts, owner
cancellation, crashes and protocol errors retain up to the current preview tail
in the existing Python tool error. They remain failures and preserve their named
diagnostic; partial output never turns an unsuccessful call into success. Output
still inside the child or in unread protocol frames may be lost. Abandoning the
entire caller future has no final response, but previously delivered updates are
not retracted. Existing process-tree cleanup and language-scoped state retirement
remain in force; completed side effects are not rolled back.

Cells use context-owned output streams. An awaited child task or
`await asyncio.to_thread(print, 'message')` can print in its originating cell.
A retained task, a saved old stdout handle, or an unattributed background thread
cannot print into a later cell's result: it gets `EVAL_OUTPUT_CLOSED`. Direct
operating-system stdout writes and subprocess output still follow the existing
private-protocol-channel policy, rather than being silently granted access to the
protocol pipe. Rich displays retain their separate owning-thread requirement.

The protocol reader independently caps complete encoded frames at 16 MiB and
queues only one complete frame. Oversized, incomplete or non-UTF-8 frames retire
the kernel with a protocol diagnostic, not an invented timeout. This keeps room
for bounded rich-display payloads as well as the smaller console frames.

JavaScript console streaming, full-output artifact persistence, background eval
jobs and a combined timestamp log are not implemented by this change. Existing
JavaScript rich displays and both languages' cancellation/reset behavior remain
unchanged. This continues the OMP-inspired eval work, not full OMP parity.

Regression tests live in `src/eval/output.rs` and `src/eval/transport.rs` and
exercise the actual public `EvalTool` as well as deterministic capture/codec
cases. The authoritative quality entry point remains
`dsr quality --tool pi_agent_rust`. Python-only subprocess probes do not establish
Rust integration, compilation, platform acceptance, or a passing DSR gate.
