# Python progress during native tool waits

Python's awaited `tool.call` now has an output-servicing Rust host as well as an
asynchronous Python reply reader. Console and rich displays produced while a
native tool is pending reach the existing running ToolUpdate callback rather
than waiting for that tool to return. No new tool option is required.

The host polls the same native tool future while draining Python's existing
one-slot reader channel. This permits an application callback to react to live
Python output even when the tool itself is waiting. It also relieves output
backpressure instead of leaving the Python event loop blocked behind a full
stdout pipe. Callback delivery retains its ordinary 50 ms coalescing policy.

Only the current cell's stream frames are admitted during that wait. The
ordinary capture still checks exact envelopes, cell identity, sequences, MIME
payloads and byte limits. Console archives receive each frame once. A malformed
frame, premature completion, second request received during the wait, read
failure or EOF stops the exchange; none is dispatched as another tool or
interpreted as success. Already received output remains subject to the existing
partial-output/error handling.

Each drain slice admits at most eight frames and targets at most 256 KiB before
polling the native operation again. A complete rich frame is indivisible and
may exceed that target, but retains the existing transport and content caps.
The native-tool wait adds no output queue, output history or reader thread.
The same read/grep/find/ls whitelist and direct-tool implementations apply.

Cancellation, the original cell deadline, and conversation ownership are
checked around draining, callback delivery, native operation polls and reply
publication. The outer lifecycle watchdog still bounds pending identity
resolution. Synchronous callbacks and synchronous portions of native tools
remain cooperative; this change cannot preempt arbitrary blocking user code.
Abandoning the wait drops the native future and the checked-out kernel under
their existing owners. Completed effects are not rolled back or replayed.

## Input delivery under the same deadline

Initial cell requests and native tool replies are now written by one job in the
existing blocking-I/O pool, not a blocking write on the async executor. The job
owns the unique input pipe and one immutable encoded frame. At most one input
write is in flight for a checked-out kernel. JSON encoding is bounded to 16 MiB,
including the newline; oversized input is refused rather than partially sent.

The executor keeps polling cancellation, ownership, output, and the original
cell deadline while waiting for the write receipt. A failed, cancelled, expired
or abandoned send never returns its pipe to the reusable kernel. The kernel is
retired, and the request is not replayed. An independently implemented observed
idle-exit restart remains allowed before any new request delivery; this work
preserves that distinction. Ordinary partial OS writes continue at the accepted
byte offset, not at the beginning of the frame.

A Python receiver may read the complete frame and emit its next request or final
response before the writer receipt is scheduled. The host retains that first
non-stream frame for its normal validation/dispatch path, and stops reading past
it until the receipt arrives. It does not classify a legitimate early response
as corruption or dispatch another tool before the current write completes.
This adds at most one bounded incoming frame alongside the existing reader
channel, not a second unbounded transcript. Console/display frames before it
still pass through the same capture exactly once.

Dropping the send revokes future write chunks. An already-running blocking-I/O
job can outlive the await until its current OS write returns. The existing owned
process-tree cleanup closes ordinary inherited readers, which permits that write
to unwind; queued jobs check revocation before writing. No forceful thread
termination or preemptive OS-I/O guarantee is introduced. Arbitrary synchronous
callbacks, process cleanup and escaped external pipe holders retain their
existing platform/cooperative limits. The input pipe is never borrowed by a new
cell while a previous job still owns it.

## Validation

Nine authored Rust cases in `src/eval/bridge_io/tests.rs` cover output-gated
native work, bounded drain fairness, malformed/misaddressed frames, transport
failure, quiet deadlines, and cancellation/session changes around publication.
The subprocess case drives the actual Python kernel and production output pump,
then a native ReadTool, with an explicit test-only delay at the native boundary.
It verifies that a rich progress callback releases the pending operation and
that initial and final console markers appear once. It is not a remote service
or model test. These Rust cases have not been compiled or executed here.

The required quality command was attempted and returned command not found
(exit 127):

```sh
dsr quality --tool pi_agent_rust
```

Rust compilation, formatting, Clippy, runtime execution and cross-platform
acceptance remain unverified. Source hashes, scoped whitespace, preserved root
regressions and embedded Python fixture syntax were checked independently;
those checks do not substitute for DSR acceptance.

Ten additional authored regressions in `src/eval/bridge_io/delivery/tests.rs`
cover partial/interrupted writes, no replay after partial/flush failures,
revocation and deadline checks, encoded-frame limits, and early response ordering.
Two use public EvalTool: a controlled non-reading interpreter peer to exercise
the hard deadline during a large request, and a real Python retained-state/read
workflow. The former is a transport fault fixture, not Python environment
validation. All nineteen new Rust cases remain uncompiled and unexecuted here.
The second DSR attempt also returned command not found (exit 127). No runtime
pass is inferred from the fixture syntax, source review or upload hash checks.
