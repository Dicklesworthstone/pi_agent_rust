# Native GDB debugging

The `debug` tool has a registered `gdb` backend using GDB's built-in DAP
interpreter over the existing owned stdio transport. This is not MI parsing,
a shell wrapper, or another debugger session manager. All ordinary breakpoint,
thread, stepping, inspection, cancellation and conversation-ownership logic
is shared with the other native adapters.

Use a Python-enabled GDB 16 or newer (17 or newer on Windows). Installing a
binary named `gdb` is not evidence that DAP is present: the normal initialize
handshake must succeed. Pi does not download GDB or silently switch to its
interactive console/MI protocol when initialization fails. Native automatic
selection still prefers LLDB; GDB is considered when no LLDB executable is
available. An explicit adapter ID always wins.

## Launch and inspect

```json
{"action":"launch","program":"target/debug/my-app","adapter":"gdb","stopOnEntry":true}
```

For GDB, Pi's `stopOnEntry:true` means a temporary stop at the beginning of the
main subprogram, not the dynamic loader's first instruction. The wire request
uses `stopAtBeginningOfMainSubprogram:true` and `stopOnEntry:false`. An explicit
`stopOnEntry:false` clears both stop flags. Initial source breakpoints still go
through the shared pre-execution configuration transaction.

```json
{"action":"stack_trace"}
```

```json
{"action":"evaluate","expression":"value","context":"watch"}
```

Supply `context:"watch"` for language expressions. GDB interprets the `repl`
context as a GDB CLI command; for example, `expression:"print value"` with
`context:"repl"`. Evaluation may execute code in the debuggee; it is not a
read-only operation. Frame and variable references returned by Pi are opaque
session handles, not raw GDB IDs. Refresh them after execution resumes.

## Attach and detach

```json
{"action":"attach","pid":12345,"adapter":"gdb"}
```

```json
{"action":"disconnect"}
```

PID attachment maps to GDB's native `pid` argument. `disconnect` preserves an
attached target; `terminate` requests termination using the existing DAP
capability and ownership checks. Remote `port` attachment remains the existing
debugpy-only path. This feature does not add GDB remote targets or core files.

## Startup and customization

The built-in GDB argv selects `--interpreter=dap`, suppresses init files with
`--nx`, disables auto-loaded scripts with `-iex "set auto-load off"`, and turns
off the inferior startup shell. Windows adds `--binary-output` to preserve DAP
frame byte counts. These flags do not sandbox GDB or the target: both are
process-execution tools. Automatic script loading is intentionally disabled,
so project pretty-printer scripts are not imported implicitly.

The explicit `adapterConfig` mechanism described in
`debug-adapter-configuration.md` also works with `adapter:"gdb"`. Override
only `command` to select a different GDB installation while retaining the
built-in argv/profile. Supplying configuration `args` replaces adapter argv
entirely, including its startup restrictions; review that executable
configuration. Tool-call `args` remain the debuggee's argv, not GDB options.
GDB does not gain Pi's debugpy/LLDB-only `runInTerminal` support.

## Native regression coverage

`src/debug/adapter_configuration_tests.rs` sends actual `DebugTool::execute`
calls through the Rust runtime to the repository's deterministic Python DAP
peer. Cases cover configured launch and breakpoint ordering, distinct attach
defaults and PID spelling, nonterminating detach, fail-before-spawn behavior,
failed-start process cleanup and relaunch, directory target opt-in, GDB stop
policy translation, and side-effect-free host-override selection. The peer
is explicitly a protocol fixture, not a real GDB debugger.

A separate opt-in `real_gdb_stops_steps_inspects_and_rejects_expired_handles`
test compiles a tiny C target with `cc`, launches actual GDB, steps through a
variable update, evaluates it, rejects a stale frame handle and terminates.
It requires GDB, a C compiler and permission to debug child processes. To
include this live-backend lane in authoritative validation, run on a DSR host:

```sh
PI_DEBUG_REQUIRE_GDB=1 PI_DEBUG_REQUIRE_PROTOCOL=1 dsr quality --tool pi_agent_rust
```

Without the opt-in, the live test emits an explicit structured skip event;
protocol tests fail instead of skipping missing Python when
`PI_DEBUG_REQUIRE_PROTOCOL` is set. These tests were authored but not executed
on the implementation host: DSR, Rust and GDB were unavailable there. A clean
DSR report and live-backend result are still required before claiming tested
GDB acceptance.

## Protocol references

The implementation follows the GDB manual's Debugger Adapter Protocol,
Command Interpreters and Auto-loading sections, and the GDB profile in OMP
v18.10.4. It deliberately retains Pi's explicit configuration authority and
shared runtime rather than copying upstream discovery behavior wholesale.
