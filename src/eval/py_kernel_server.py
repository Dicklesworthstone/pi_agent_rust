"""pi eval tool: persistent Python kernel server (bd-cv653.1.4).

JSON-lines protocol over stdio:
  request:  {"id": <int>, "code": "<source>"}
  response: {"id": <int>, "ok": true,  "stdout": "...", "stderr": "...", "result": "<repr|null>"}
            {"id": <int>, "ok": false, "stdout": "...", "stderr": "...", "error": "<traceback>"}

One persistent namespace and asyncio loop across cells. Top-level await,
async for, and async with are supported; use await instead of asyncio.run().
The last expression is displayed without implicitly awaiting a coroutine
value. The host owns timeouts by killing this process. Pending tasks are
retained, but the loop only advances while a cell is executing.

Tool re-entry bridge: cell code can call `tool.read(path)`, `tool.grep(...)`,
`tool.find(...)`, `tool.ls(...)`, or `await tool.call("read", {"path": path})`.
The kernel emits a {"bridge": {"call": m, "tool": ..., "input": {...}}} line
on the private stdout channel and reads the host's {"bridge_result": ...}
before resuming. Bridge exchanges are serialized, including async calls;
permissions and the tool whitelist remain owned by the host.
"""

import ast
import asyncio
import contextvars
import inspect
import io
import json
import os
import sys
import traceback

NAMESPACE = {"__name__": "__main__"}
# Protocol-channel protection: the host treats every stdout line as protocol,
# so cell code (or its subprocesses, which inherit fd 1 by default) writing to
# the REAL fd 1 would kill the kernel — or worse, forge/desync protocol
# frames. Duplicate the protocol channel onto a private fd and point fd 1 at
# /dev/null; `sys.stdout` capture still works because run_cell swaps the
# Python-level object.
_REAL_STDOUT = os.fdopen(os.dup(1), "w", encoding="utf-8")
os.dup2(os.open(os.devnull, os.O_WRONLY), 1)
sys.stdout = sys.__stdout__ = os.fdopen(1, "w", encoding="utf-8")
# Likewise protect stdin: a cell subprocess reading fd 0 could steal a
# bridge_result frame and desync every later call.
_REAL_STDIN = os.fdopen(os.dup(0), "r", encoding="utf-8")
os.dup2(os.open(os.devnull, os.O_RDONLY), 0)
sys.stdin = sys.__stdin__ = os.fdopen(0, "r", encoding="utf-8")
_BRIDGE_CALL = 0
_CELL_LOOP = asyncio.new_event_loop()
asyncio.set_event_loop(_CELL_LOOP)
_CELL_CONTEXT = contextvars.copy_context()


class ToolBridgeError(RuntimeError):
    pass


def _bridge_call(tool_name, tool_input):
    global _BRIDGE_CALL
    _BRIDGE_CALL += 1
    call_id = _BRIDGE_CALL
    _REAL_STDOUT.write(
        json.dumps({"bridge": {"call": call_id, "tool": tool_name, "input": tool_input}}) + "\n"
    )
    _REAL_STDOUT.flush()
    line = _REAL_STDIN.readline()
    if not line:
        raise ToolBridgeError("bridge closed")
    reply = json.loads(line)
    result = reply.get("bridge_result", {})
    if result.get("call") != call_id:
        raise ToolBridgeError("bridge call id mismatch")
    if not result.get("ok", False):
        raise ToolBridgeError(result.get("error", "tool call failed"))
    return result.get("content", "")


class _PiTools:
    """`tool` object exposed to cells: whitelisted re-entry into pi tools."""

    @staticmethod
    async def call(name, arguments):
        """Await a host tool with JSON-object arguments; host policy still applies.

        Deliberately do not offload stdio to a thread: a cancelled worker could
        otherwise consume the next cell's request as its bridge reply. The
        host processes one exchange at a time, also under asyncio.gather().
        """
        if not isinstance(name, str) or not isinstance(arguments, dict):
            raise TypeError("tool.call requires a tool name and an argument object")
        return _bridge_call(name, arguments)

    @staticmethod
    def read(path, **kwargs):
        payload = {"path": path}
        payload.update(kwargs)
        return _bridge_call("read", payload)

    @staticmethod
    def grep(pattern, path=None, **kwargs):
        payload = {"pattern": pattern}
        if path is not None:
            payload["path"] = path
        payload.update(kwargs)
        return _bridge_call("grep", payload)

    @staticmethod
    def find(pattern, **kwargs):
        payload = {"pattern": pattern}
        payload.update(kwargs)
        return _bridge_call("find", payload)

    @staticmethod
    def ls(path=".", **kwargs):
        payload = {"path": path}
        payload.update(kwargs)
        return _bridge_call("ls", payload)


NAMESPACE["tool"] = _PiTools()


def _compile_cell(code):
    tree = ast.parse(code, filename="<cell>", mode="exec")
    trailing = None
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        trailing = ast.Expression(tree.body[-1].value)
        tree.body = tree.body[:-1]
    # Compile BOTH pieces before executing either. In particular, a syntax
    # error in a final yield/await must not leave prefix side effects behind.
    flags = ast.PyCF_ALLOW_TOP_LEVEL_AWAIT
    body = compile(tree, "<cell>", "exec", flags=flags, dont_inherit=True)
    expression = (
        compile(trailing, "<cell>", "eval", flags=flags, dont_inherit=True)
        if trailing is not None
        else None
    )
    return body, expression


async def _execute_cell(code):
    global _CELL_CONTEXT
    try:
        body, expression = _compile_cell(code)
        value = eval(body, NAMESPACE)  # noqa: S307 - intentional cell execution
        if body.co_flags & inspect.CO_COROUTINE:
            await value
        if expression is None:
            return True, None, None
        value = eval(expression, NAMESPACE)  # noqa: S307
        # Only await compiler-generated top-level-await code, not an arbitrary
        # awaitable that happens to be the final value (normal REPL semantics).
        if expression.co_flags & inspect.CO_COROUTINE:
            value = await value
        return True, repr(value) if value is not None else None, None
    except BaseException:  # noqa: BLE001 - SystemExit must not stop the loop
        return False, None, traceback.format_exc()
    finally:
        # Each run_until_complete creates a task; keep the cell's context
        # rather than silently resetting ContextVars to the host's context.
        _CELL_CONTEXT = contextvars.copy_context()


def run_cell(code):
    stdout = io.StringIO()
    stderr = io.StringIO()
    old_out, old_err = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = stdout, stderr
    try:
        ok, result, error = _CELL_CONTEXT.run(
            _CELL_LOOP.run_until_complete, _execute_cell(code)
        )
        return ok, stdout.getvalue(), stderr.getvalue(), result, error
    except BaseException:  # noqa: BLE001 - full traceback back to the host
        return False, stdout.getvalue(), stderr.getvalue(), None, traceback.format_exc()
    finally:
        sys.stdout, sys.stderr = old_out, old_err


def main():
    while True:
        line = _REAL_STDIN.readline()
        if not line:
            break
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
            ok, out, err, result, error = run_cell(request.get("code", ""))
            response = {
                "id": request.get("id"),
                "ok": ok,
                "stdout": out,
                "stderr": err,
            }
            if ok:
                response["result"] = result
            else:
                response["error"] = error
        except Exception as exc:  # noqa: BLE001 - protocol-level failure
            response = {"id": None, "ok": False, "stdout": "", "stderr": "", "error": str(exc)}
        _REAL_STDOUT.write(json.dumps(response) + "\n")
        _REAL_STDOUT.flush()


if __name__ == "__main__":
    main()
