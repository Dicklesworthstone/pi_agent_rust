# Persistent eval kernels

The `eval` tool runs one cell at a time in a retained Python or QuickJS kernel.
Pi's input fields are `code`, `kernel` (`python` or `js`, default `python`), and
`timeout_secs` (default 30, clamped to 1–600). These are Pi's native fields, not
OMP's `language`/`timeout` wire format. Python requires a usable interpreter;
`PI_EVAL_PYTHON` selects it instead of `python3`.

## Async Python cells

Python cells share variables, imports, one asyncio event loop, and the cell's
ContextVars context. Top-level `await`, `async for`, and `async with` work:

```json
{"kernel":"python","code":"import asyncio\nanswer = await asyncio.sleep(0, result=42)\nanswer"}
```

A later cell can use `answer` or await a task/future created in an earlier cell.
The loop advances only while a cell runs; retained tasks are **not** managed
background jobs and do not run while the kernel waits for the next request.
Use `await`, not `asyncio.run()`, inside a cell. A coroutine returned as the last
expression is displayed, not implicitly executed; explicitly await it.

The whole cell is compiled before it executes. Syntax errors therefore do not
run an earlier statement in that cell. Runtime exceptions preserve mutations
that have already occurred. Python `SystemExit` and `CancelledError` are
reported as cell errors without discarding the kernel. A host timeout or process
crash still discards its state and reports that the next call starts fresh.

### Tool calls

Existing synchronous `tool.read(path)`, `tool.grep(pattern, ...)`,
`tool.find(pattern, ...)`, and `tool.ls(path)` calls remain available. Async code
can use JSON-object arguments:

```json
{"kernel":"python","code":"text = await tool.call('read', {'path': 'README.md'})\ntext[:200]"}
```

The Rust host still permits only `read`, `grep`, `find`, and `ls` on this bridge.
`tool.call` does not expose arbitrary tools, MCP, writes, or subagent delegation.
Exchanges are serialized even when wrapped in `asyncio.gather`; the coroutine
API is not parallel host-tool execution. Host read/path errors are raised back
into the cell. The eval tool itself executes arbitrary code and is not a sandbox.

## Reusable Python scripts with `%load`

A standalone cell can load a workspace-local Python script:

```json
{"kernel":"python","code":"%load \"analysis/setup data.py\""}
```

The script executes in the same retained namespace and asyncio loop. It can
use top-level await and the tool bridge. Later cells reuse its definitions and
variables. An explicit second `%load` reads and executes the current file again;
editing the file alone does not reload it. Imported modules retain ordinary
Python import-cache semantics.

The script's resolved filename is used in tracebacks and assigned to `__file__`.
Its directory is retained at the front of `sys.path` for sibling imports,
including imports inside functions called in later cells. Compilation happens
before changing `__file__` or `sys.path`; runtime failures, like ordinary cells,
do not roll back partial execution.

Paths resolve against the Python kernel's original workspace, even after a cell
calls `os.chdir()`. Quote paths containing spaces; use forward slashes or quoted
paths on Windows. Loads accept regular files up to 2 MiB, honor Python source
encoding cookies and UTF-8 BOMs, and reject resolved paths outside the workspace.
URL schemes, including `http://`, `local://`, and `artifact://`, are not executable
through this command. Download and inspect remote code separately before saving
it locally. Directories and special files are rejected. File identity/metadata
checks detect observed changes during reading; they are not an atomic snapshot
against a hostile concurrent writer or a security boundary for arbitrary code.

`%load` must occupy the entire cell and take exactly one path. It never invokes
a shell. `%pip`, `%bun`, JS file loading, and automatic dependency installation
are not implemented by this Python command.

## Async JavaScript cells

JavaScript retains ordinary script-level `let`, `const`, function declarations,
and globals. Top-level `await` uses QuickJS's async grammar; declarations made
inside that mode are not persistent, so use `globalThis` for values to retain.
Grammar selection happens before execution: runtime `SyntaxError` exceptions,
including failed `JSON.parse` calls, never re-execute the cell's earlier work.

```json
{"kernel":"js","code":"await sleep(10); globalThis.text = await tool.call('read', {path:'README.md'}); console.log(text.slice(0,200))"}
```

`tool.call(name, args)` returns a promise for the same content string returned
by the existing synchronous helpers. It accepts object arguments. Host policy
still permits only `read`, `grep`, `find`, and `ls`; rejected calls reject an
`Error` that can be caught with `try`/`catch` or `.catch()`. `Promise.all` can
compose calls, but the asynchronous bridge dispatches them one at a time and
the host still executes them serially. `tool.read(path)` and the other positional
synchronous helpers keep their existing return types.

`sleep(milliseconds)`, `setTimeout(callback, milliseconds, ...args)`,
`clearTimeout(id)`, `setInterval`, and `clearInterval` are available. Timers run
while the cell waits for its returned promise, with promise microtasks before
timer callbacks. Timer callbacks must be functions; strings are never evaluated.
Negative delays clamp to zero; non-finite delays and values above 2147483647 ms
are rejected. Intervals have a minimum delay of one millisecond.

**Await the work to keep it alive.** Outstanding timers and undispatched async
calls are discarded when a cell returns or fails. They are not retained tasks,
and their promises cannot be resumed in a later cell. An already dispatched
host read may finish; discarding a cell does not undo admitted host work.
There is no timer thread, background execution between cells, network API,
module loader, or expanded filesystem authority added by these helpers.

The reactor admits at most 128 pending operations, 4096 operation creations,
and 65536 delivered events per cell. Tool arguments are limited to 256 KiB each
and 1 MiB across queued async calls. Limits fail explicitly rather than dropping
part of a request. Callbacks execute without holding the native registry borrow.

A JavaScript timeout now discards the realm and reports `EVAL_STATE_RESET`;
the next cell starts with fresh variables. This deliberately replaces the old
state-preserved timeout behavior: abandoned promise continuations must not run
under the next cell's deadline. Ordinary errors with no leftover microtasks
preserve completed mutations. Legacy synchronous bridge waits now obey the cell
deadline too, and dropping the kernel revokes its worker. These are cooperative
boundaries, not hard real-time guarantees or rollback of completed work.

Kernel regressions live under `src/eval/js_kernel/`; `tests/eval_async_js.rs`
exercises these helpers through the actual Rust `EvalTool`. JavaScript-bootstrap
probes under Node do not establish QuickJS, Rust-host, or DSR acceptance.

## Scope and validation

These additions target the persistent-async and file-backed-cell capabilities
in OMP v18.10.4's `docs/tools/eval.md`; they do not claim complete OMP eval parity.
Structured rich displays, tracked auto-backgrounding, general session-tool and
subagent bridges, and the Bun runtime remain separate work.

Regression coverage is in `tests/eval.rs`, through the actual Rust `EvalTool`.
Use the repository's authoritative `dsr quality --tool pi_agent_rust` workflow.
Standalone Python subprocess probes exercise the kernel protocol, but do not
establish Rust integration, platform parity, or a passing DSR gate.
