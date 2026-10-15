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
There is no new output queue, output history, detached task or reader thread.
The same read/grep/find/ls whitelist and direct-tool implementations apply.

Cancellation, the original cell deadline, and conversation ownership are
checked around draining, callback delivery, native operation polls and reply
publication. The outer lifecycle watchdog still bounds pending identity
resolution. Synchronous callbacks and synchronous portions of native tools
remain cooperative; this change cannot preempt arbitrary blocking user code.
Abandoning the wait drops the native future and the checked-out kernel under
their existing owners. Completed effects are not rolled back or replayed.

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
