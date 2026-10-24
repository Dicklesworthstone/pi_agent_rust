# Recoverable Python soft deadlines

Python eval can opt into an earlier, recoverable deadline without relaxing the
existing hard whole-cell budget. This is a Rust-native slice of OMP-inspired
retained-kernel interruption, not full cancellation parity.

```json
{
  "kernel": "python",
  "code": "steps = 0\nwhile True:\n    steps += 1",
  "soft_timeout_secs": 5,
  "timeout_secs": 30
}
```

On a clean soft stop, the call returns an error result containing
`EVAL_SOFT_TIMEOUT`. The next cell can inspect `steps` and variables/imports from
earlier cells without reloading their data. Completed mutations are **not**
rolled back. An interrupted library operation may leave inconsistent objects;
use `reset: true` when continuing from that state would be unsafe.

## Two budgets, one unchanged hard limit

`soft_timeout_secs` is optional, Python-only, and currently supported on POSIX
hosts with Python interval timers. It must be an integer from 1 through 599,
strictly below the effective `timeout_secs` (which retains its existing 1–600
clamp and 30-second default). JavaScript, non-POSIX hosts, and invalid values
are rejected explicitly before execution admission or reset. Omitting the
field preserves ordinary hard-deadline behavior.

The soft clock starts when the Python runner admits the cell, before source
preparation or execution; kernel startup and transport still consume the
original native hard budget. Neither queuing a tool, package installation,
output production, nor cleanup pauses or extends the hard deadline. Set enough
space between the deadlines for cooperative cleanup. A soft timeout is not a
guaranteed maximum return latency: native code, guest exception handlers, and
an in-flight tool reply can defer completion. The original hard watchdog still
retires the owned process tree when that budget expires.

User cancellation, abandonment, and conversation-ownership loss continue to
use the existing fail-closed retirement path. This feature does not make
Ctrl+C recoverable and does not change JavaScript interruption semantics.

## Reuse is a validated outcome

Synchronous Python receives an internal soft-stop exception; top-level await
also receives cancellation through its explicitly tracked root task. Normal
`finally` cleanup and owned plot finalization can run. Catching the exception
or suppressing asyncio cancellation cannot turn an expired cell into success.
Host-tool admission closes for the expired cell, including queued calls.

Only a settled root with completed runner finalization, a disarmed timer, and
valid complete output may return its kernel to the retained slot. Its native
result details contain:

```json
{
  "errorKind": "soft_timeout",
  "softTimeout": true,
  "stateReset": false
}
```

These fields are attached only after the existing native final-response and
rich-output validators accept the response. Cell numbering continues on reuse.
A `kernelError` or `bridgeError` marker instead forces native retirement and
reports state loss; it never asserts `stateReset: false`. Unsolicited or
non-boolean soft-timeout metadata and a timed-out success response are rejected.

Retained detached guest tasks and Python threads are not a sandbox or a global
quiescence guarantee. Their old output/tool contexts remain revoked, but guest
code can still mutate ordinary Python objects or use its existing filesystem
and process access. This feature proves the runner root and owned tool channel
settled, not that arbitrary application state is transactional.

## Tool calls and package processes

The existing cooperative `await tool.call(...)` implementation is retained:
only one addressed reply reader owns stdin, with bounded immutable queued
requests. A soft signal never jumps out between request publication and reader
ownership. It requests cancellation, lets the existing dispatched reply drain,
and prevents queued work from being sent under the expired cell's authority.
Synchronous helpers also finish their owned exchange under the hard watchdog.

Soft expiry **during an active tool exchange** conservatively requires process
retirement even after a valid reply drains. A missing reply remains bounded by
the native hard deadline; no second reader, replay, or fresh deadline is added.
A completed tool call followed by a later computation-only soft stop can retain
state. Requests are checked natively for current-cell identity, exact envelope,
positive exchange ID, and existing request/name limits before tool dispatch.
IDs allocated before async queueing need not arrive in numerical order.

A soft-timed-out cell that entered the runner's package installer also requires
process-tree retirement, including when pip appeared to finish before the
computation timed out. This conservative boundary avoids promising that helper
processes or installer cleanup are safe to retain. Package effects already
performed are not rolled back.

## Signal ownership

Opt-in uses Python's POSIX `SIGALRM`/`ITIMER_REAL`. A pre-existing custom handler,
active interval timer, or blocked alarm signal is refused before queuing cell
code; it is not overwritten or cancelled. Numeric/platform validation happens
before reset, but this interpreter-side signal check occurs after any explicit
reset requested for the call.

After first opt-in, the kernel keeps its alarm dispatcher installed until reset
so late delivery cannot kill the idle runner. The dispatcher ignores idle,
expired, or premature alarms and checks the current cell's monotonic deadline.
Each active timer is disarmed at the cell boundary. Do not repurpose SIGALRM or
its timer inside a soft-deadline cell. Guest code that overrides signal behavior
is not sandboxed; the native hard deadline remains the final boundary.

## Verification

`tests/eval_python_soft_timeout.py` launches the actual bundled Python server.
It covers synchronous/async interruption, retained namespace/process identity,
ContextVars, cleanup, caught cancellation, delayed alarms, timer conflicts,
protocol draining, queued-call refusal, package retirement, and plot output.
`tests/eval_python_bridge.py` and the preceding scientific-output substrate
suite exercise the unchanged bridge/output behavior alongside it.

`tests/eval_soft_timeout.rs` exercises the public EvalTool API, including the
unchanged hard watchdog and native retirement; transport unit tests cover
admission and metadata. These Rust tests are authored, not executed in the
implementation environment: `dsr quality --tool pi_agent_rust` could not start
because DSR was unavailable (exit 127). Rust compilation, formatting, Clippy,
native-host, frontend, and cross-platform acceptance remain unverified. Python
subprocess results are not a substitute for the required DSR quality lane.
