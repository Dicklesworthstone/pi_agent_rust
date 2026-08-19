"""pi eval tool: persistent Python kernel server (bd-cv653.1.4).

JSON-lines protocol over stdio:
  request:  {"id": <int>, "code": "<source>"}
  response: {"id": <int>, "ok": true,  "stdout": "...", "stderr": "...", "result": "<repr|null>"}
            {"id": <int>, "ok": false, "stdout": "...", "stderr": "...", "error": "<traceback>"}

One persistent namespace across cells. The last statement of a cell, when it
is an expression, is evaluated separately and its repr returned (Jupyter-like
display semantics). The host owns timeouts by killing this process.
"""

import ast
import io
import json
import sys
import traceback

NAMESPACE = {"__name__": "__main__"}


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
    for line in sys.stdin:
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
