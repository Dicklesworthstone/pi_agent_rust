# Interactive debuggee terminals

Local `debug` launches with the registered `debugpy` or `lldb-dap` adapter can
opt into a host-owned pseudo-terminal:

```json
{"action":"launch","program":"app.py","runInTerminal":true,"stopOnEntry":false}
```

This enables programs that require terminal stdin/stdout/stderr, including
Python `input()` workflows. The adapter's `runInTerminal` reverse request starts
the command in a real 120-column by 40-row PTY. No intermediate shell is added;
argument strings, including empty arguments, reach the selected executable
verbatim. An explicitly selected executable can itself interpret its arguments.
This is not a sandbox or a restriction on the already trusted local adapter.

Initial breakpoints and exception filters still use the normal DAP startup
handshake. Stack inspection, variables, evaluation, assignment and stepping
use the same session and suspension-scoped handles. A terminal does not add a
second debugger or change the approval/effect classification of `debug`.

## Reading output and entering input

Read the adapter output and the separate, labelled terminal tail:

```json
{"action":"output"}
```

Send a line to the program without evaluating it in the debugger:

```json
{"action":"terminal_input","text":"answer-42"}
```

`enter` defaults to true and appends a carriage return. Set `enter:false` to
send only the supplied text, including deliberate terminal control characters.
Each request accepts at most 1024 UTF-8 bytes without NUL, plus the optional
carriage return. An empty string with `enter:true` sends an empty line. Terminal
line discipline and the target decide echoing, newline conversion and when
input becomes visible. Input can remain queued in the PTY while the debugger
has suspended the program; continue execution for the program to consume it.

`bytesWritten` acknowledges a completed write to the PTY, not application
consumption. Four input frames may wait in the native input queue. Each async
wait is cancellable and limited to 30 seconds. Cancellation drops an unsent
frame; a write that already began can still complete, including partially before
an error. There is no automatic retry or rollback of typed input.

Terminal stdout/stderr share one 32 KiB byte tail. `output` also returns the
execution snapshot, whose `terminal` metadata includes state, PID, exit code
when known, observed/retained byte counts, capture completion and read errors.
Truncated prefixes are labelled. UTF-8 decoding happens after chunk assembly;
a truncated leading scalar or non-UTF-8 output may use replacement characters.
Adapter and terminal tails are separate captures, not a globally ordered log.
No full-output artifact, scrollback file, VT screen emulator, resizing, dedicated
TUI pane, external terminal window or binary stdin API is added.

## Authority and lifetime

`runInTerminal` is opt-in and **local-launch-only**. PID attachment and external
loopback debugpy attachment never advertise local terminal-launch support.
They cannot enable it through `custom_request`. Delve and other adapter IDs are
not admitted by this public option. The existing default launch path is unchanged.

Only the pending launch request services reverse calls. The DAP reader admits
bounded detached JSON; host work runs on the original async request with its
original capability context. The launch callback accepts `runInTerminal` only.
Child-session `startDebugging` and shell-interpreted arguments remain refused.
An adapter that completes the opted-in launch without obtaining a terminal
produces `DAP_TERMINAL_NOT_STARTED`, not a claimed terminal success.

One terminal may be started per DAP session. Reverse input admits at most 128
arguments, 128 environment changes and 64 KiB of encoded arguments. Environment
strings override inherited values; null removes a variable. The requested cwd
must resolve to an existing directory inside the original canonical workspace.
The executable itself need not live there, so installed interpreters and adapter
launchers work. This cwd check is not an atomic hostile-writer snapshot or a
filesystem security boundary. No environment values are added to terminal
status metadata, although the program can print them like any other process.

The terminal belongs to the original conversation's DapSession. Switching away
does not pause or kill its program; switching back restores its input and tail.
Other conversations cannot use that terminal through their `debug` tool. The
existing four-debugger conversation capacity remains, without target eviction.

Failed or cancelled startup, debugger connection retirement, explicit termination
and session/tool drop revoke the terminal owner. A dedicated worker retains the
actual child handle through process-tree cleanup and reap, using the existing
native cleanup discipline. Input/output operations never signal copied PIDs.
These are cooperative lifecycle boundaries, not synchronous rollback or a
promise to stop descendants that deliberately escape process containment.

At most eight terminal lifetimes exist process-wide. A slot remains occupied
until its owner, input and output lanes have all ended, even after an abandoned
startup or when an escaped descendant retains a PTY. This bounds native lane
accumulation; it does not make blocking OS operations preemptible. Capacity
exhaustion is an explicit `DAP_TERMINAL_CAPACITY` refusal, not target eviction.

The normal initialization and startup budgets still apply. Running a terminal
is part of the launch budget, not an unlimited detached request. After successful
launch, its lifetime is the debugger session, not the launch request's timer.

## Validation

`tests/debug_terminal.rs` exercises public registry/tool launch, real PTYs,
literal argv, input/output, conversation switch-back, failed-start cleanup and
dormant-session drop against a deterministic DAP peer. The same file contains
an explicitly ignored live-debugpy workflow selected through DSR with
`PI_DEBUG_TEST_PYTHON` (default `python3`). Unit tests cover input policy, bounded
Unicode capture, cancellation before spawn and cancellation after real startup.

An independent Python DAP/PTY development probe against debugpy 1.8.20 exercised
terminal launch, all three terminal streams, stdin, source breakpoints, inspection,
assignment and cleanup. That probe did not execute Rust or portable-pty and does
not establish platform parity. Rust compilation, tests, formatting and Clippy
require the repository's authoritative `dsr quality --tool pi_agent_rust` lane.
