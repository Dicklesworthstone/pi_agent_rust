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

## Scope and validation

These additions target the persistent-async and file-backed-cell capabilities
in OMP v18.10.4's `docs/tools/eval.md`; they do not claim complete OMP eval parity.
Structured rich displays, tracked auto-backgrounding, general session-tool and
subagent bridges, and the Bun runtime remain separate work.

Regression coverage is in `tests/eval.rs`, through the actual Rust `EvalTool`.
Use the repository's authoritative `dsr quality --tool pi_agent_rust` workflow.
Standalone Python subprocess probes exercise the kernel protocol, but do not
establish Rust integration, platform parity, or a passing DSR gate.
