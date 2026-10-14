# Persistent eval kernels

The `eval` tool runs one cell at a time in a retained Python or QuickJS kernel.
Pi's input fields are `code`, `kernel` (`python` or `js`, default `python`),
`reset` (boolean, default false), and `timeout_secs` (default 30, clamped to
1–600). These are Pi's native fields, not
OMP's `language`/`timeout` wire format. Python requires a usable interpreter;
`PI_EVAL_PYTHON` selects it instead of `python3`.

## Exclusive execution and reset

Calls on one `EvalTool` instance execute exclusively, including direct SDK calls
and calls selecting different languages. Waiting is asynchronous and does not
consume the active cell budget; simultaneous callers have no FIFO guarantee.
Separate instances retain independent state and admission. Dropping a waiting
future executes nothing. Dropping an active future discards its checked-out
kernel through the existing cleanup path; it does not roll back completed work.

Use `reset: true` to retire only the selected language before executing its code:

```json
{"kernel":"js","reset":true,"code":""}
```

An empty cell performs reset without additional user code. The other language
is untouched. Requests are validated before reset, so invalid language names,
non-boolean reset values or malformed fields cannot silently discard state.
Successful reset requests report `EVAL_STATE_RESET`, `details.reset: true`, and
a new cell sequence starting at one. Backend errors after reset still disclose
the state loss. Ordinary results include `details.reset: false`.

## Python rich displays

Python's `display(value, *, mime=None)` captures an explicit output snapshot.
Strings default to `text/plain`; other supported JSON values default to
`application/json`. Choose `text/markdown` or `image/png` explicitly:

```python
display({"answer": 42, "observations": [True, None]})
display("## Result\nThe calculation completed.", mime="text/markdown")

# Plotting libraries are optional. After saving a figure to an io.BytesIO:
display(png_buffer.getvalue(), mime="image/png")
```

PNG inputs are bytes/bytearray or canonical standard base64, never paths or
URLs. The helper does not read files, fetch images, inspect notebook MIME hooks,
or install plotting packages. JSON accepts built-in dictionaries with string
keys, lists/tuples, strings, booleans, null, finite floats, and integers fitting
signed/unsigned 64-bit storage. Convert larger integers to strings explicitly.
Custom objects and serializers are not invoked; convert them deliberately.

Display snapshots are returned in call order as native content blocks. JSON also
appears without text parsing in `details.jsonOutputs`; `details.displays` records
MIME types, encoded byte counts and PNG dimensions without duplicating base64.
Captured stdout/stderr precede the displays, and the final expression or error
follows them. This is final-cell output, not live streaming or a combined
stdout/display timestamp log. A display preceding an ordinary exception remains
available, but the tool result is still an error. A timeout/crash may lose the
unreturned cell's captures. The final expression's existing `repr()` is unchanged.

Each cell admits at most 32 displays, 2 MiB per serialized MIME frame, and 4 MiB
across frames. JSON values are limited to depth 32 and 65,536 nodes. PNG envelope
checks require header/end markers, dimensions at most 16,384 per axis and at
most 16,777,216 pixels; neither helper nor host decompresses or certifies the
raster. These are display limits, not bounds on arbitrary eval memory or stdout.
Limits reject the offending display explicitly; they never silently truncate.
The Rust host independently validates the complete collection before publishing
any of it or returning the kernel to its reusable slot.

Only the active cell's thread/context may display. A retained task from a prior
cell keeps that prior sink, which closes at its boundary; late display attempts
raise `EVAL_DISPLAY_CLOSED` rather than polluting another result. Awaiting a child
task within its originating cell works. This does not turn retained tasks into
managed background jobs. JavaScript has the separate helper described below.

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
a shell or installs dependencies. `%bun` is not a Python command.

## Python packages with `%pip install`

Install dependencies without replacing the retained Python process:

```json
{"kernel":"python","code":"%pip install pillow","timeout_secs":600}
```

Later cells can import installed packages while retaining variables, the asyncio
loop and ContextVars. Only explicit standalone `%pip install` is supported; a
missing import suggests this command but never installs anything automatically.
Distribution and import names can differ, such as `pillow` and `PIL`.

The launcher uses the kernel's captured interpreter, not a `pip` found on PATH.
Python isolated mode prevents a workspace `pip.py` or `PYTHONPATH` from replacing
the installer. Pip must already be installed in that interpreter's environment;
user-site-only pip is not visible to isolated mode. Pi does not bootstrap pip,
create a virtual environment, invoke a shell, or inject `--break-system-packages`.
Select a suitable virtual-environment interpreter using `PI_EVAL_PYTHON` before
starting the session. Pip's own configuration and the supplied install options
still control package sources and destinations; this is not an installation
sandbox, an approval bypass, or a reproducible dependency resolver.

Requirements, wheels and option values use shell-style quoting, without shell
execution. Relative paths always resolve from the original kernel workspace,
even after `os.chdir()`. For example, an offline installation can use
`%pip install --no-index --no-deps 'wheel house/example-1.0-py3-none-any.whl'`.
The command is limited to 16 KiB and 128 nonempty install arguments. Requirements
files remain pip inputs, not executable eval cells. Non-install subcommands,
malformed quoting, multiline commands and NUL bytes are rejected.

Installer stdout and stderr flow through the existing sequenced output capture,
live `ToolUpdate` previews and console archives. They are consumed concurrently
in bounded chunks, not accumulated in a second unbounded subprocess buffer.
`EVAL_PIP_START` and `EVAL_PIP_DONE` identify launcher progress; a nonzero pip exit
returns `EVAL_PIP_FAILED` with captured diagnostics, never a successful cell.
Ordinary installer failures leave the namespace usable. There is no automatic
retry, and a failed or interrupted install may have changed packages on disk.

**The normal active cell timeout applies, including downloads and builds.**
Unlike OMP's paused package watchdog, this implementation does not pause or
extend the budget. Use an explicit `timeout_secs` for longer installs, up to the
existing 600-second limit. Host timeout/cancellation retires the selected kernel
and its owned process tree, including the installer. This loses Python variables
but cannot roll back completed installation effects. Build hooks are ordinary
package-manager code with the current user's privileges, not sandboxed scripts.

Import discovery caches are refreshed after pip exits, including a failed exit
that may have installed some dependencies. Already imported modules are not
reloaded, `sys.path` is not rewritten, and `.pth` hooks are not replayed. An
explicit `--target`, newly created user-site directory, editable installation or
new import hook can require a deliberate path change or kernel reset before use.
Package files remain on disk across a kernel reset; the reset only discards
runtime state. An explicit `reset:true` applies before the Python command, even
when that command later fails validation or installation.

Offline integration regressions are in `tests/eval_python_packages.rs`; their
inert wheel installs use private temporary targets, never the test interpreter's
global package directory. Python-only subprocess probes establish kernel behavior,
not Rust-host, cross-platform or DSR acceptance.

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

## JavaScript rich displays

`display(value, mimeType = "application/json")` returns explicit JSON snapshots,
plain text, Markdown, and PNG images. Unlike Python, strings also default to
JSON; pass `"text/plain"` to display them without JSON quotes.

```javascript
const observations = {answer: 42, checks: [true, null]};
display(observations);
observations.answer = 0; // the captured snapshot still contains 42
display("## Result\nChecks completed.", "text/markdown");
await sleep(1);
display(pngBytes, "image/png"); // Uint8Array or canonical standard base64
```

No paths, URLs, notebook MIME hooks, or implicit `toJSON()` methods are opened
or invoked. JSON accepts null, booleans, finite numbers, well-formed Unicode
strings, dense arrays, and plain/null-prototype objects with enumerable string
data properties. Cycles, accessors, custom instances, enumerable symbols,
undefined, bigint, NaN and infinities are refused instead of silently losing
data. Convert unsupported values explicitly. Reflection on a Proxy can still
run its traps under the cell deadline; eval is not a sandbox.

The same Rust formatter and MIME validator serve both kernels. Displays retain
call order, images become native image blocks, and JSON values are available in
`details.jsonOutputs`. Captured console output precedes displays, and the final
expression or exception follows them; ordinary final-expression semantics are
unchanged. Valid displays preceding an ordinary exception remain available in
an errored result. The byte/count/structure and PNG-envelope limits above apply
equally. Invalid PNG envelopes are rejected by the host as a whole-result error,
and that kernel is discarded. Raster decompression/validation is not performed.

Calls capture into the currently executing JavaScript cell. Awaited timer and
promise callbacks can display while it runs; the sink closes when it returns.
The existing reactor retires unawaited timers and requests, and a timeout resets
the realm. Deliberately invoking a saved function in a later cell uses that
later cell's sink; this is not Python's originating-task ContextVar contract.
These are final-cell captures, not live output, artifact spill, detached tasks,
or a general memory bound on JavaScript or console output. Regression coverage
is shared in `tests/eval_display.rs`, with native sink/bootstrap tests under
`src/eval/js_kernel/display.rs`.

## Scope and validation

These additions target the persistent-async and file-backed-cell capabilities
in OMP v18.10.4's `docs/tools/eval.md`; they do not claim complete OMP eval parity.
Tracked auto-backgrounding, general session-tool and subagent bridges, and the
Bun runtime remain separate work. Python and JavaScript rich-output
coverage is in `tests/eval_display.rs` and `src/eval/display.rs`.

Regression coverage is in `tests/eval.rs`, through the actual Rust `EvalTool`.
Use the repository's authoritative `dsr quality --tool pi_agent_rust` workflow.
Standalone Python subprocess probes exercise the kernel protocol, but do not
establish Rust integration, platform parity, or a passing DSR gate.
