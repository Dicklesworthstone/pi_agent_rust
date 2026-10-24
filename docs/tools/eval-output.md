# Python and JavaScript eval streaming output

Both `eval` kernels deliver native `ToolUpdate` snapshots while a cell runs.
This uses the same callback consumed by interactive, RPC and SDK tool execution,
not a separate command or output polling tool. A snapshot replaces the previous
preview; consumers must not append each complete snapshot as a delta.

```json
{"kernel":"python","code":"import asyncio\nprint('Starting', flush=True)\nawait asyncio.sleep(1)\nprint('Finished')\n42"}
```

```json
{"kernel":"js","code":"console.log('Starting'); await sleep(1000); console.warn('Finished'); 42"}
```

The host normally coalesces updates at 50 ms intervals and flushes pending output
before host-tool waits and final completion. Each preview keeps at most 32 KiB of
UTF-8 data from each of stdout and stderr, plus explicit omission notices.
Callback execution is synchronous on the host's eval polling path, never inside
the QuickJS worker; callbacks should return promptly. Coalescing is not a hard
real-time delivery guarantee. No separate output timer thread or orphan output
task is created.

JavaScript `console.log`, `console.info` and `console.debug` use stdout;
`console.error` and `console.warn` use stderr. Arguments retain the existing
string-coercion semantics, separated by spaces and followed by a newline.
This is not a claim of Node/Bun's full console formatting API.

## One capture, bounded storage

Python sends complete, sequenced output frames rather than retaining unbounded
`StringIO` objects. The host admits at most 64 KiB of decoded text in each stream
frame and verifies the cell ID and contiguous sequence. The final Python
response must confirm that sequence and may not contain a second console copy.

JavaScript uses the same host capture through a dedicated native channel for
that invocation, holding at most eight frames of 64 KiB each. Small console
calls produce one frame; large calls are split without breaking Unicode scalars.
A full channel applies backpressure and checks the cell deadline and worker
revocation while waiting. The native sender closes before the final reply; the
host drains its remaining frames before assembling the result. Guest text is
never parsed as transport protocol. The host also drains console output while
an asynchronous JavaScript tool bridge request is waiting for native I/O.

Final results retain a bounded head and tail: at most 512 KiB of source UTF-8
bytes per stream, plus a labelled `EVAL_OUTPUT_TRUNCATED` notice in the omitted
middle. Small output is unchanged within each stream. Unicode scalars are never
split or reordered. This is sampled output, **not a persisted full-output
artifact**. Omitted data is not recoverable from the tool result. Execution can
succeed even when its console rendering is sampled; sampling is disclosed
separately from success or failure.

`details.output` uses schema `pi.eval.output.v1` and records `sequence`,
`stdoutBytes`, `stderrBytes`, `stdoutOmittedBytes`, `stderrOmittedBytes`,
`truncated`, and `preview`. Counts describe the output received by the host.
Omission counts describe that particular preview or final rendering and exclude
the notices themselves. Pure expression cells that emit no console frames retain
their previous details shape. Update metadata identifies the selected kernel as
`python` or `js`.

Both languages now group stdout before stderr in final text, followed by the
existing rich displays and final expression or exception. JavaScript no longer
interleaves log and error calls in a single console buffer. This is not a combined
stdout/stderr/display timestamp log. Rich JSON, Markdown and PNG displays remain
final-cell captures, independently validated by the existing shared formatter;
they are not duplicated in console updates.

Final Python `repr` results above 1 MiB are refused with `EVAL_RESULT_LIMIT`, not
silently truncated. Oversized Python tracebacks keep an explicitly labelled
bounded tail. These Python-specific rules are unchanged. Console limits do not
bound arbitrary guest allocations, string coercion, custom serialization work,
or JavaScript final-expression serialization.

## Cancellation, failure, and retained state

The per-call capture lives outside the active kernel future. Timeouts, owner
cancellation, crashes and protocol errors preserve a bounded preview of already
received console output. Python retains its existing tool-error convention;
JavaScript watchdog stops remain unsuccessful `ToolOutput` results with their
named diagnostic and state-reset metadata first. Partial results are labelled
by language, and JavaScript partial-result byte counts describe the actual
preview rather than the larger retained capture. Ordinary guest exceptions
preserve console output and do not by themselves discard persistent state.

Partial output never turns an unsuccessful call into success. Output still in
the producer or in unread channel frames may be lost. Abandoning the entire
caller future has no final response, but previously delivered updates are not
retracted. Existing process-tree cleanup and language-scoped state retirement
remain in force; completed side effects are not rolled back. Output callbacks
that request cancellation are checked before starting another bridge operation.

Python cells use context-owned output streams. An awaited child task or
`await asyncio.to_thread(print, 'message')` can print in its originating cell.
A retained task, a saved old stdout handle, or an unattributed background thread
cannot print into a later cell's result: it gets `EVAL_OUTPUT_CLOSED`. Direct
operating-system stdout writes and subprocess output still follow the existing
private-protocol-channel policy. Rich displays retain their separate
owning-thread requirement.

The Python protocol reader independently caps complete encoded frames at 16 MiB
and queues only one complete frame. Oversized, incomplete or non-UTF-8 frames
retire the kernel with a protocol diagnostic, not an invented timeout. This
keeps room for bounded rich-display payloads as well as smaller console frames.

JavaScript output channels are replaced for every cell. Existing cell-owned
timers and pending tool replies are cleared at completion; timed-out realms are
discarded so stale continuations cannot run under a later cell's budget. Worker
revocation also interrupts a producer waiting on a full console channel.

Full-output artifact persistence, managed background eval jobs, generalized
session-tool/subagent re-entry and a combined timestamp log remain outside this
implementation. The existing read-only bridge whitelist is unchanged. This
continues the OMP-inspired eval work, not full OMP or Bun runtime compatibility.

Regression tests live in `src/eval/output.rs`, `src/eval/transport.rs` and
`src/eval/js_kernel/console.rs`. They exercise the public `EvalTool` as well as
bounded capture, protocol and backpressure cases. The authoritative quality
entry point remains `dsr quality --tool pi_agent_rust`. Authored regressions and
source review do not establish Rust compilation, integration, platform
acceptance or a passing DSR gate.
