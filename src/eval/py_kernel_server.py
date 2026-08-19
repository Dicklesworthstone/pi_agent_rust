"""pi eval tool: persistent Python kernel server (bd-cv653.1.4).

JSON-lines protocol over stdio:
  request:  {"id": <int>, "code": "<source>"}
  response: {"id": <int>, "ok": true,  "stdout": "...", "stderr": "...", "result": "<repr|null>"}
            {"id": <int>, "ok": false, "stdout": "...", "stderr": "...", "error": "<traceback>"}

One persistent namespace across cells. The last statement of a cell, when it
is an expression, is evaluated separately and its repr returned (Jupyter-like
display semantics). The host owns timeouts by killing this process.

Tool re-entry bridge: cell code can call `tool.read(path)`, `tool.grep(...)`,
`tool.find(...)`, `tool.ls(...)` — the kernel emits a
{"bridge": {"call": m, "tool": ..., "input": {...}}} line on the REAL stdout
and blocks reading the host's {"bridge_result": ...} line from stdin before
resuming the cell. Policy identical to direct tool calls (the host executes
the same tool implementations).
"""

import ast
import io
import json
import sys
import traceback

NAMESPACE = {"__name__": "__main__"}
_REAL_STDOUT = sys.stdout
_BRIDGE_CALL = 0


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
    line = sys.stdin.readline()
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


def run_cell(code):
    stdout = io.StringIO()
    stderr = io.StringIO()
    result_repr = None
    old_out, old_err = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = stdout, stderr
    try:
        tree = ast.parse(code, mode="exec")
        trailing = None
        if tree.body and isinstance(tree.body[-1], ast.Expr):
            trailing = ast.Expression(tree.body[-1].value)
            tree.body = tree.body[:-1]
        if tree.body:
            exec(compile(tree, "<cell>", "exec"), NAMESPACE)  # noqa: S102
        if trailing is not None:
            value = eval(compile(trailing, "<cell>", "eval"), NAMESPACE)  # noqa: S307
            if value is not None:
                result_repr = repr(value)
        return True, stdout.getvalue(), stderr.getvalue(), result_repr, None
    except BaseException:  # noqa: BLE001 - full traceback back to the host
        return False, stdout.getvalue(), stderr.getvalue(), None, traceback.format_exc()
    finally:
        sys.stdout, sys.stderr = old_out, old_err


def main():
    while True:
        line = sys.stdin.readline()
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
        sys.stdout.write(json.dumps(response) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
