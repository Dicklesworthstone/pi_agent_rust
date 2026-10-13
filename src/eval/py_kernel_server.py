"""pi eval tool: persistent Python kernel server (bd-cv653.1.4).

JSON-lines protocol over stdio:
  request:  {"id": <int>, "code": "<source>"}
  stream:   {"id": <int>, "stream": {"sequence": <int>, "name": "stdout|stderr", "text": "..."}}
  response: {"id": <int>, "ok": true, "stdout": "", "stderr": "", "result": "<repr|null>", "outputSequence": <int>}
            {"id": <int>, "ok": false, "stdout": "", "stderr": "", "error": "<traceback>", "outputSequence": <int>}

Console bytes stream to the host in bounded, ordered frames, never duplicated
in the final response. The host owns previews and bounded head/tail capture.
Context-copied workers may print while their owning cell is active; closed
contexts and unattributed background threads cannot emit into later cells.

One persistent namespace and asyncio loop across cells. Top-level await,
async for, and async with are supported; use await instead of asyncio.run().
The last expression is displayed without implicitly awaiting a coroutine
value. The host owns timeouts by killing this process. Pending tasks are
retained, but the loop only advances while a cell is executing.

A standalone `%load "relative/path with spaces.py"` cell executes a local
Python script in that same namespace and loop. Paths resolve from the kernel's
original workspace, not a later os.chdir(). Scripts retain their filename in
tracebacks, set __file__, and put their directory on sys.path for imports.
Load again to execute an edited script; ordinary Python import caching still
applies to its dependencies. Loads are limited to regular workspace files of
at most 2 MiB. This admission check is not a sandbox for arbitrary eval code.

Tool re-entry bridge: cell code can call `tool.read(path)`, `tool.grep(...)`,
`tool.find(...)`, `tool.ls(...)`, or `await tool.call("read", {"path": path})`.
The kernel emits a {"bridge": {"call": m, "tool": ..., "input": {...}}} line
on the private stdout channel and reads the host's {"bridge_result": ...}
before resuming. Bridge exchanges are serialized, including async calls;
permissions and the tool whitelist remain owned by the host.
"""

import ast
import asyncio
import base64
import contextvars
import inspect
import io
import json
import math
import os
from pathlib import Path
import shlex
import stat
import sys
import threading
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
_WORKSPACE_ROOT = Path.cwd().resolve()
_MAX_SCRIPT_BYTES = 2 * 1024 * 1024
_DISPLAY_CONTEXT = contextvars.ContextVar("pi_eval_display", default=None)
_MAX_DISPLAYS = 32
_MAX_DISPLAY_ITEM_BYTES = 2 * 1024 * 1024
_MAX_DISPLAY_BYTES = 4 * 1024 * 1024
_OUTPUT_CONTEXT = contextvars.ContextVar("pi_eval_output", default=None)
_MAX_RESULT_BYTES = 1024 * 1024
_PROTOCOL_WRITE_LOCK = threading.RLock()


def _send_frame(frame):
    encoded = json.dumps(frame, ensure_ascii=False, separators=(",", ":")) + "\n"
    with _PROTOCOL_WRITE_LOCK:
        _REAL_STDOUT.write(encoded)
        _REAL_STDOUT.flush()


class _CellOutput:
    """No unbounded kernel-side stdout buffer; the host owns bounded capture."""

    def __init__(self, cell_id):
        self.cell_id = cell_id
        self.sequence = 0
        self.active = True

    def write(self, name, text):
        if not self.active or _OUTPUT_CONTEXT.get() is not self:
            raise RuntimeError("EVAL_OUTPUT_CLOSED: output belongs to an inactive cell")
        # Slice BEFORE encoding: a large write cannot create another equally
        # large capture/JSON string. Surrogates use the explicit printable
        # backslash form rather than corrupting the host's UTF-8 protocol.
        for offset in range(0, len(text), 8192):
            chunk = text[offset:offset + 8192].encode("utf-8", "backslashreplace").decode("utf-8")
            # Context-copied, awaited workers (asyncio.to_thread) may print.
            # Serialize complete frames with bridge/final replies and close
            # the sink under this same lock at the cell boundary.
            with _PROTOCOL_WRITE_LOCK:
                if not self.active:
                    raise RuntimeError("EVAL_OUTPUT_CLOSED: output belongs to an inactive cell")
                self.sequence += 1
                _send_frame({"id": self.cell_id, "stream": {
                    "sequence": self.sequence, "name": name, "text": chunk,
                }})
        return len(text)


class _CellStream(io.TextIOBase):
    encoding = "utf-8"
    errors = "backslashreplace"

    def __init__(self, owner, name):
        self.owner = owner
        self.name = name

    def writable(self):
        return True

    def write(self, text):
        if self.closed:
            raise ValueError("I/O operation on closed eval stream")
        if not isinstance(text, str):
            raise TypeError("eval stream write() requires str")
        return self.owner.write(self.name, text)

    def flush(self):
        # Every write already flushes its protocol frames. No thread, timer,
        # queued fragment, or post-cell flush can leak output into another cell.
        return None


def _result_text(value):
    text = repr(value)
    if len(text) > _MAX_RESULT_BYTES:
        raise ValueError("EVAL_RESULT_LIMIT: final repr exceeds 1 MiB; print or display a bounded selection")
    encoded = text.encode("utf-8", "backslashreplace")
    if len(encoded) > _MAX_RESULT_BYTES:
        raise ValueError("EVAL_RESULT_LIMIT: final repr exceeds 1 MiB; print or display a bounded selection")
    return encoded.decode("utf-8")


def _error_text():
    text = traceback.format_exc()
    # Traceback construction and arbitrary guest repr allocations are not
    # sandboxed. Bound what is retained/serialized into the protocol response.
    if len(text) > _MAX_RESULT_BYTES // 8:
        text = "EVAL_ERROR_TRUNCATED: traceback tail follows\n" + text[-(_MAX_RESULT_BYTES // 8):]
    return text.encode("utf-8", "backslashreplace").decode("utf-8")


def _display_json_shape(value, depth=0, remaining=None):
    """Accept JSON data without calling custom serializers or coercing keys."""
    if remaining is None:
        remaining = [65536]
    remaining[0] -= 1
    if depth > 32 or remaining[0] < 0:
        raise ValueError("EVAL_DISPLAY_LIMIT: JSON nesting or node budget exceeded")
    kind = type(value)
    if kind is str:
        if len(value) > _MAX_DISPLAY_ITEM_BYTES:
            raise ValueError("EVAL_DISPLAY_LIMIT: string exceeds the item budget")
    elif kind is int:
        if not -(1 << 63) <= value < (1 << 64):
            raise ValueError("EVAL_DISPLAY_JSON: large integers must be explicit strings")
    elif kind is float:
        if not math.isfinite(value):
            raise ValueError("EVAL_DISPLAY_JSON: non-finite numbers are not JSON")
    elif kind in (list, tuple):
        if len(value) > remaining[0]:
            raise ValueError("EVAL_DISPLAY_LIMIT: JSON node budget exceeded")
        for item in value:
            _display_json_shape(item, depth + 1, remaining)
    elif kind is dict:
        if len(value) > remaining[0]:
            raise ValueError("EVAL_DISPLAY_LIMIT: JSON node budget exceeded")
        for key, item in value.items():
            if type(key) is not str:
                raise TypeError("EVAL_DISPLAY_JSON: object keys must be strings")
            if len(key) > _MAX_DISPLAY_ITEM_BYTES:
                raise ValueError("EVAL_DISPLAY_LIMIT: JSON key exceeds the item budget")
            _display_json_shape(item, depth + 1, remaining)
    elif kind not in (bool, type(None)):
        raise TypeError("EVAL_DISPLAY_JSON: use JSON values or an explicit text MIME type")


def _display_png(value):
    if type(value) not in (str, bytes, bytearray):
        raise TypeError("EVAL_DISPLAY_PNG: expected PNG bytes or standard base64")
    if len(value) > _MAX_DISPLAY_ITEM_BYTES:
        raise ValueError("EVAL_DISPLAY_LIMIT: PNG exceeds the item budget")
    if type(value) is str:
        raw = base64.b64decode(value, validate=True)
        encoded = base64.b64encode(raw).decode("ascii")
        if encoded != value:
            raise ValueError("EVAL_DISPLAY_PNG: use canonical standard base64")
    else:
        raw = value
        encoded = base64.b64encode(raw).decode("ascii")
    # Envelope admission only; no raster decompression in the kernel or host.
    if (len(raw) < 45 or raw[:16] != b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"
            or raw[-12:] != b"\x00\x00\x00\x00IEND\xaeB`\x82"):
        raise ValueError("EVAL_DISPLAY_PNG: missing PNG header or end marker")
    width = int.from_bytes(raw[16:20], "big")
    height = int.from_bytes(raw[20:24], "big")
    if not 0 < width <= 16384 or not 0 < height <= 16384 or width * height > 16777216:
        raise ValueError("EVAL_DISPLAY_PNG: dimensions exceed the display budget")
    return encoded


class _DisplaySink:
    def __init__(self):
        self.frames = []
        self.bytes = 0
        self.active = True
        self.thread = threading.get_ident()

    def append(self, value, mime):
        if not self.active or threading.get_ident() != self.thread:
            raise RuntimeError("EVAL_DISPLAY_CLOSED: display belongs to an inactive cell")
        if len(self.frames) >= _MAX_DISPLAYS:
            raise ValueError("EVAL_DISPLAY_LIMIT: at most 32 displays per cell")
        if mime is None:
            mime = "text/plain" if type(value) is str else "application/json"
        if mime == "image/png":
            data = _display_png(value)
        elif mime in ("text/plain", "text/markdown"):
            if type(value) is not str:
                raise TypeError("EVAL_DISPLAY_TEXT: explicit text displays require a string")
            data = value
        elif mime == "application/json":
            data = value
        else:
            raise ValueError("EVAL_DISPLAY_MIME: supported types are text/plain, text/markdown, application/json, image/png")
        _display_json_shape(data)
        frame = {"mimeType": mime, "data": data}
        encoder = json.JSONEncoder(ensure_ascii=False, allow_nan=False, separators=(",", ":"))
        chunks = []
        size = 0
        for chunk in encoder.iterencode(frame):
            size += len(chunk.encode("utf-8"))
            if size > _MAX_DISPLAY_ITEM_BYTES or self.bytes + size > _MAX_DISPLAY_BYTES:
                raise ValueError("EVAL_DISPLAY_LIMIT: encoded display byte budget exceeded")
            chunks.append(chunk)
        # Snapshot at the call, not at cell completion. Later mutations cannot
        # change an already displayed JSON object. Refused items change nothing.
        snapshot = json.loads("".join(chunks))
        self.frames.append(snapshot)
        self.bytes += size


def display(value, *, mime=None):
    """Capture explicit JSON, text, Markdown or PNG output for this cell."""
    sink = _DISPLAY_CONTEXT.get()
    if sink is None:
        raise RuntimeError("EVAL_DISPLAY_CLOSED: no active cell")
    sink.append(value, mime)


NAMESPACE["display"] = display


class ToolBridgeError(RuntimeError):
    pass


def _bridge_call(tool_name, tool_input):
    global _BRIDGE_CALL
    _BRIDGE_CALL += 1
    call_id = _BRIDGE_CALL
    _send_frame({"bridge": {"call": call_id, "tool": tool_name, "input": tool_input}})
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


def _script_stamp(metadata):
    return (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def _prepare_cell(code):
    if not isinstance(code, str):
        raise TypeError("cell code must be a string")
    command = code.strip()
    if not command.startswith("%"):
        return code, "<cell>", None
    if "\n" in command or "\r" in command:
        raise ValueError("EVAL_LOAD_USAGE: %load must be a standalone cell")
    try:
        arguments = shlex.split(command)
    except ValueError as error:
        raise ValueError("EVAL_LOAD_USAGE: quote the script path correctly") from error
    if len(arguments) != 2 or arguments[0] != "%load" or not arguments[1]:
        raise ValueError("EVAL_LOAD_USAGE: %load <workspace-local Python file>")
    if "://" in arguments[1] or "\x00" in arguments[1]:
        raise ValueError("EVAL_LOAD_PATH: URLs and virtual documents cannot be executed")
    path = Path(arguments[1])
    if not path.is_absolute():
        path = _WORKSPACE_ROOT / path
    path = path.resolve(strict=True)
    try:
        path.relative_to(_WORKSPACE_ROOT)
    except ValueError as error:
        raise ValueError("EVAL_LOAD_PATH: script is outside the kernel workspace") from error
    before = path.stat()
    if not stat.S_ISREG(before.st_mode):
        raise ValueError("EVAL_LOAD_PATH: script must be a regular file")
    if before.st_size > _MAX_SCRIPT_BYTES:
        raise ValueError("EVAL_LOAD_LIMIT: script exceeds 2 MiB")
    # Read once, bounded even if the file grows. NONBLOCK prevents a raced-in
    # FIFO from hanging; NOFOLLOW rejects replacement of the resolved leaf
    # with a symlink on platforms that support it. Stamp checks detect observed
    # changes, not an atomic snapshot against a hostile workspace writer.
    flags = os.O_RDONLY
    for name in ("O_CLOEXEC", "O_NONBLOCK", "O_NOFOLLOW"):
        flags |= getattr(os, name, 0)
    with os.fdopen(os.open(path, flags), "rb") as script:
        opened = os.fstat(script.fileno())
        if not stat.S_ISREG(opened.st_mode) or _script_stamp(opened) != _script_stamp(before):
            raise ValueError("EVAL_LOAD_CHANGED: script changed while opening")
        source = script.read(_MAX_SCRIPT_BYTES + 1)
        after = os.fstat(script.fileno())
        if _script_stamp(after) != _script_stamp(opened):
            raise ValueError("EVAL_LOAD_CHANGED: script changed while reading")
    if len(source) > _MAX_SCRIPT_BYTES:
        raise ValueError("EVAL_LOAD_LIMIT: script exceeds 2 MiB")
    # Compile bytes so Python honors encoding cookies and UTF-8 BOMs.
    return source, str(path), str(path.parent)


def _compile_cell(code, filename):
    tree = ast.parse(code, filename=filename, mode="exec")
    trailing = None
    if tree.body and isinstance(tree.body[-1], ast.Expr):
        trailing = ast.Expression(tree.body[-1].value)
        tree.body = tree.body[:-1]
    # Compile BOTH pieces before executing either. In particular, a syntax
    # error in a final yield/await must not leave prefix side effects behind.
    flags = ast.PyCF_ALLOW_TOP_LEVEL_AWAIT
    body = compile(tree, filename, "exec", flags=flags, dont_inherit=True)
    expression = (
        compile(trailing, filename, "eval", flags=flags, dont_inherit=True)
        if trailing is not None
        else None
    )
    return body, expression


async def _execute_cell(code, displays, output):
    global _CELL_CONTEXT
    display_token = _DISPLAY_CONTEXT.set(displays)
    output_token = _OUTPUT_CONTEXT.set(output)
    try:
        source, filename, script_dir = _prepare_cell(code)
        body, expression = _compile_cell(source, filename)
        if script_dir is not None:
            # Only mutate execution metadata after the whole script compiles.
            # Keep it for later calls to functions defined by this script.
            NAMESPACE["__file__"] = filename
            if script_dir in sys.path:
                sys.path.remove(script_dir)
            sys.path.insert(0, script_dir)
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
        return True, _result_text(value) if value is not None else None, None
    except BaseException:  # noqa: BLE001 - SystemExit must not stop the loop
        return False, None, _error_text()
    finally:
        # Child tasks retain this particular sink. Once the cell ends they
        # cannot publish late output into a later cell's display collection.
        _DISPLAY_CONTEXT.reset(display_token)
        _OUTPUT_CONTEXT.reset(output_token)
        # Each run_until_complete creates a task; keep the cell's context
        # rather than silently resetting ContextVars to the host's context.
        _CELL_CONTEXT = contextvars.copy_context()


def run_cell(code, cell_id):
    displays = _DisplaySink()
    output = _CellOutput(cell_id)
    stdout = _CellStream(output, "stdout")
    stderr = _CellStream(output, "stderr")
    old_out, old_err = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = stdout, stderr
    try:
        ok, result, error = _CELL_CONTEXT.run(
            _CELL_LOOP.run_until_complete, _execute_cell(code, displays, output)
        )
    except BaseException:  # noqa: BLE001 - full traceback back to the host
        ok, result, error = False, None, _error_text()
    finally:
        displays.active = False
        with _PROTOCOL_WRITE_LOCK:
            output.active = False
            sequence = output.sequence
        sys.stdout, sys.stderr = old_out, old_err
    return ok, result, error, displays.frames, sequence


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
            ok, result, error, displays, sequence = run_cell(request.get("code", ""), request.get("id"))
            response = {
                "id": request.get("id"),
                "ok": ok,
                "stdout": "",
                "stderr": "",
                "displays": displays,
                "outputSequence": sequence,
            }
            if ok:
                response["result"] = result
            else:
                response["error"] = error
        except Exception as exc:  # noqa: BLE001 - protocol-level failure
            response = {"id": None, "ok": False, "stdout": "", "stderr": "", "error": str(exc)}
        _send_frame(response)


if __name__ == "__main__":
    main()
