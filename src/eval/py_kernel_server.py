"""pi eval tool: persistent Python kernel server (bd-cv653.1.4).

JSON-lines protocol over stdio:
  request:  {"id": <int>, "code": "<source>"}
  stream:   {"id": <int>, "stream": {"sequence": <int>, "name": "stdout|stderr", "text": "..."}}
  display:  {"id": <int>, "stream": {"sequence": <display index>, "name": "display", "display": {"mimeType": "...", "data": ...}}}
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

A standalone `%pip install <requirements/options>` cell runs pip using this
kernel's interpreter, without a shell or a replacement kernel. Installer
stdout/stderr use the existing cell-owned output frames. The ordinary active
cell timeout still applies; installations do not pause or extend it. Relative
paths resolve from the original workspace. Package/build side effects are not
rolled back, and already imported modules are not reloaded automatically.

Tool re-entry bridge: cell code can call `tool.read(path)`, `tool.grep(...)`,
`tool.find(...)`, `tool.ls(...)`, or `await tool.call("read", {"path": path})`.
The kernel emits a {"bridge": {"call": m, "tool": ..., "input": {...}}} line
on the private stdout channel and reads the host's {"bridge_result": ...}
before resuming. Bridge exchanges are serialized, including async calls;
awaited exchanges let the cell's asyncio tasks run while the host replies.
Cancellation after dispatch drains that one reply under the original host
deadline before returning; it never abandons a reader into a later cell.
Permissions and the tool whitelist remain owned by the host.
"""

import ast
import asyncio
import base64
import codecs
import contextvars
import functools
import importlib
import inspect
import io
import json
import math
import os
from pathlib import Path
import queue
import shlex
import shutil
import stat
import sys
import tempfile
import threading
import traceback
import weakref

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
_BRIDGE_EXCHANGE = threading.Lock()
_BRIDGE_BUSY = False
_BRIDGE_ENCODING = False
_BRIDGE_FAILURE = None
_BRIDGE_READER = None
_MAX_BRIDGE_CALLS = 64
_MAX_BRIDGE_REQUEST_BYTES = 1024 * 1024
_MAX_BRIDGE_REPLY_BYTES = 16 * 1024 * 1024
_CELL_LOOP = asyncio.new_event_loop()
asyncio.set_event_loop(_CELL_LOOP)
_BRIDGE_LOCK = asyncio.Lock()
_CELL_CONTEXT = contextvars.copy_context()
_WORKSPACE_ROOT = Path.cwd().resolve()
_KERNEL_PYTHON = os.path.abspath(sys.executable)
_MAX_SCRIPT_BYTES = 2 * 1024 * 1024
_DISPLAY_CONTEXT = contextvars.ContextVar("pi_eval_display", default=None)
_MAX_DISPLAYS = 32
_MAX_DISPLAY_ITEM_BYTES = 2 * 1024 * 1024
_MAX_DISPLAY_BYTES = 4 * 1024 * 1024
_OUTPUT_CONTEXT = contextvars.ContextVar("pi_eval_output", default=None)
_MAX_RESULT_BYTES = 1024 * 1024
_PROTOCOL_WRITE_LOCK = threading.RLock()
# Host ownership outlives Python's process group/Windows Job, including when
# cancellation kills this process before Python cleanup can run.
_PACKAGE_ROOT = os.environ.pop("_PI_EVAL_PACKAGE_ROOT", None)
_PACKAGE_LAYERS = []
_MAX_PACKAGE_LAYERS = 32
# Optional plotting libraries are imported only by user code. Respect an
# explicit backend choice; otherwise do not require a display server.
os.environ.setdefault("MPLBACKEND", "Agg")


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
        self.thread = threading.get_ident()
        self.bridge_error = None
        self.bridge_open = True
        self.bridge_tasks = set()

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

    def display(self, sequence, snapshot):
        # A distinct display index leaves stdout/stderr accounting and console
        # archives unchanged. Send a complete bounded MIME snapshot, not text
        # chunks that could expose a partial JSON value or PNG to the host.
        with _PROTOCOL_WRITE_LOCK:
            if not self.active or _OUTPUT_CONTEXT.get() is not self:
                raise RuntimeError("EVAL_OUTPUT_CLOSED: display belongs to an inactive cell")
            _send_frame({"id": self.cell_id, "stream": {
                "sequence": sequence, "name": "display", "display": snapshot,
            }})


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
    _display_dimensions(width, height)
    return encoded


def _display_dimensions(width, height):
    # Also runs before optional libraries allocate/encode a raster. Fractional
    # figure pixels round up conservatively; NaN/inf fail the comparisons.
    if (not (0 < width <= 16384 and 0 < height <= 16384)
            or math.ceil(width) * math.ceil(height) > 16777216):
        raise ValueError("EVAL_DISPLAY_PNG: dimensions exceed the display budget")


class _DisplayPngBuffer(io.BytesIO):
    """Bound encoded image storage before a writer can grow the byte buffer."""

    def __init__(self, byte_budget):
        super().__init__()
        overhead = len(b'{"mimeType":"image/png","data":""}')
        self.limit = max(0, (min(byte_budget, _MAX_DISPLAY_ITEM_BYTES) - overhead) // 4 * 3)

    def write(self, data):
        if self.tell() + memoryview(data).nbytes > self.limit:
            raise ValueError("EVAL_DISPLAY_LIMIT: encoded PNG exceeds the remaining display budget")
        return super().write(data)

    def truncate(self, size=None):
        if (self.tell() if size is None else size) > self.limit:
            raise ValueError("EVAL_DISPLAY_LIMIT: encoded PNG exceeds the remaining display budget")
        return super().truncate(size)


def _loaded_instance(value, module_name, class_name):
    module = sys.modules.get(module_name)
    kind = getattr(module, class_name, None) if module is not None else None
    return isinstance(kind, type) and isinstance(value, kind)


def _scientific_representation(value, byte_budget):
    # Consult modules already imported by the user. A plain cell does not load
    # scientific packages, probe the network or install optional dependencies.
    if _loaded_instance(value, "PIL.Image", "Image"):
        _display_dimensions(*value.size)
        with _DisplayPngBuffer(byte_budget) as target:
            value.save(target, format="PNG")
            return "image/png", target.getvalue()
    if _loaded_instance(value, "matplotlib.figure", "Figure"):
        width, height = value.get_size_inches() * value.dpi
        _display_dimensions(width, height)
        with _DisplayPngBuffer(byte_budget) as target:
            # Ignore savefig DPI/tight-bbox defaults that could enlarge the
            # raster beyond preflight. Agg renders without changing the user's
            # chosen interactive canvas. Artist callbacks remain guest code.
            value.savefig(target, format="PNG", dpi=value.dpi, backend="agg",
                          bbox_inches=value.bbox_inches.frozen(), pad_inches=0)
            return "image/png", target.getvalue()
    table = value
    series = _loaded_instance(value, "pandas", "Series")
    if series:
        table = value.to_frame()
    if series or _loaded_instance(value, "pandas", "DataFrame"):
        # A bounded text view preserves NaN/timestamps/index labels without
        # pretending they are JSON or requiring optional HTML/tabulate engines.
        text = table.to_string(max_rows=40, min_rows=20, max_cols=12,
                               max_colwidth=80, show_dimensions=True)
        return "text/plain", (
            "Pandas preview (up to 40 rows, 12 columns; values up to 80 characters):\n" + text
        )
    return None


_RICH_DISPLAY_MIMES = ("image/png", "application/json", "text/markdown", "text/plain")
_PLAIN_DISPLAY_TYPES = (str, int, float, bool, type(None), list, tuple, dict)


def _rich_representation(value, byte_budget):
    """Ask a top-level object for one supported representation, never HTML.

    Formatters are guest Python code, not a sandbox or a serializer for nested
    objects. Exceptions propagate; invalid advertised data is not silently
    replaced by a weaker representation. The ordinary frame validator remains
    the only path from a formatter to the wire.
    """
    if type(value) in _PLAIN_DISPLAY_TYPES:
        return None
    scientific = _scientific_representation(value, byte_budget)
    if scientific is not None:
        return scientific
    bundle_hook = getattr(value, "_repr_mimebundle_", None)
    if callable(bundle_hook):
        bundle = bundle_hook(include=list(_RICH_DISPLAY_MIMES), exclude=[])
        if type(bundle) is tuple and len(bundle) == 2 and type(bundle[1]) is dict:
            bundle = bundle[0]  # Notebook metadata is not a host instruction.
        if bundle is not None:
            if type(bundle) is not dict or len(bundle) > 32:
                raise TypeError("EVAL_DISPLAY_BUNDLE: expected at most 32 MIME entries")
            for mime in _RICH_DISPLAY_MIMES:
                if mime in bundle and (bundle[mime] is not None or mime == "application/json"):
                    return mime, bundle[mime]
    for mime, name in (("image/png", "_repr_png_"),
                       ("application/json", "_repr_json_"),
                       ("text/markdown", "_repr_markdown_")):
        hook = getattr(value, name, None)
        if callable(hook):
            data = hook()
            if data is None:
                continue  # A hook may explicitly decline this representation.
            if type(data) is tuple and len(data) == 2 and type(data[1]) is dict:
                data = data[0]
            return mime, data
    return None


class _DisplaySink:
    def __init__(self):
        self.frames = []
        self.bytes = 0
        self.active = True
        self.thread = threading.get_ident()
        self.formatting = False
        self.plot_managers = []
        self.displayed_figures = []
        self.finishing_plots = False

    def admit_plot(self):
        self.check_available()
        if self.finishing_plots:
            raise RuntimeError("EVAL_PLOT_REENTRANT: cannot create figures during cell finalization")
        # Closed figures do not consume slots forever in a plotting loop.
        self.plot_managers = [ref for ref in self.plot_managers
                              if _registered_plot(ref())]
        if len(self.plot_managers) >= _MAX_DISPLAYS:
            raise ValueError("EVAL_DISPLAY_LIMIT: at most 32 open cell-owned figures")

    def finish_plots(self):
        # Never scan global pyplot state: only managers created in this live
        # cell can be auto-published or closed. Weak references retain neither
        # figures nor prior cell sinks after pyplot/user references disappear.
        failure = None
        self.finishing_plots = True
        try:
            for reference in self.plot_managers:
                manager = reference()
                registry = _registered_plot(manager)
                if registry is None:
                    continue
                figure = manager.canvas.figure
                try:
                    if not any(ref() is figure for ref in self.displayed_figures):
                        self.append(figure, None)
                except BaseException:
                    if failure is None:
                        failure = _error_text()
                finally:
                    # Close by identity, not reused figure number. Even an
                    # oversized or failing figure must not silently accumulate
                    # in pyplot or be retried as a later cell's result. A user
                    # variable referencing the Figure remains usable.
                    try:
                        if _registered_plot(manager) is registry:
                            registry.destroy(manager)
                    except BaseException:
                        if failure is None:
                            failure = _error_text()
        finally:
            self.finishing_plots = False
            self.plot_managers.clear()
            self.displayed_figures.clear()
        return failure

    def check_available(self):
        output = _OUTPUT_CONTEXT.get()
        if (not self.active or threading.get_ident() != self.thread
                or _DISPLAY_CONTEXT.get() is not self
                or output is None or not output.active):
            raise RuntimeError("EVAL_DISPLAY_CLOSED: display belongs to an inactive cell")
        if self.formatting:
            raise RuntimeError("EVAL_DISPLAY_REENTRANT: a formatter cannot call display recursively")
        if len(self.frames) >= _MAX_DISPLAYS:
            raise ValueError("EVAL_DISPLAY_LIMIT: at most 32 displays per cell")

    def append_result(self, value):
        # Preserve ordinary REPL repr semantics, including large integers and
        # containers. Only objects with rich representations become displays.
        if type(value) in _PLAIN_DISPLAY_TYPES:
            return False
        return self.append(value, None, rich_only=True)

    def append(self, value, mime, *, rich_only=False):
        self.check_available()  # Refuse stale/threaded calls BEFORE guest hooks.
        original = value
        if mime is None:
            self.formatting = True
            try:
                representation = _rich_representation(value, _MAX_DISPLAY_BYTES - self.bytes)
            finally:
                self.formatting = False
            if representation is not None:
                mime, value = representation
            elif rich_only:
                return False
            else:
                mime = "text/plain" if type(value) is str else "application/json"
        self.check_available()
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
        output = _OUTPUT_CONTEXT.get()
        if output is None:
            raise RuntimeError("EVAL_DISPLAY_CLOSED: no active output owner")
        output.display(len(self.frames) + 1, snapshot)
        # Only successfully sent snapshots enter the final collection. The
        # host compares that collection with the live prefix before reuse.
        self.frames.append(snapshot)
        self.bytes += size
        if _loaded_instance(original, "matplotlib.figure", "Figure"):
            self.displayed_figures.append(weakref.ref(original))
        return True


def display(value, *, mime=None):
    """Capture JSON/text/PNG or a top-level object's supported rich output.

    Explicit mime bypasses representation hooks and validates the supplied
    value directly. Omitted mime permits synchronous notebook-style hooks;
    HTML, SVG, external URLs and arbitrary object serialization stay disabled.
    """
    sink = _DISPLAY_CONTEXT.get()
    if sink is None:
        raise RuntimeError("EVAL_DISPLAY_CLOSED: no active cell")
    sink.append(value, mime)


NAMESPACE["display"] = display


def _registered_plot(manager):
    if manager is None:
        return None
    helpers = sys.modules.get("matplotlib._pylab_helpers")
    registry = getattr(helpers, "Gcf", None)
    if registry is not None and registry.figs.get(manager.num) is manager:
        return registry
    return None


def _observe_plot_managers(module):
    """Attach ownership at creation, not by guessing from process-global IDs."""
    manager_type = getattr(module, "FigureManagerBase", None)
    if manager_type is None:
        return
    original = manager_type.__init__
    if getattr(original, "_pi_eval_owned", False):
        return

    @functools.wraps(original)
    def initialize(manager, canvas, num, *args, **kwargs):
        owner = _DISPLAY_CONTEXT.get()
        matplotlib = sys.modules.get("matplotlib")
        # Leave deliberately selected non-Agg/interactive backends alone.
        # They can still use the explicit display(Figure) PNG adapter.
        if (owner is not None and matplotlib is not None
                and str(matplotlib.get_backend()).casefold() == "agg"):
            owner.admit_plot()  # Refuse stale/threaded creation before effects.
        else:
            owner = None
        original(manager, canvas, num, *args, **kwargs)
        if owner is not None:
            owner.plot_managers.append(weakref.ref(manager))

    initialize._pi_eval_owned = True
    manager_type.__init__ = initialize


class _PlotImportLoader:
    """Observe one optional module while preserving its real loader."""

    def __init__(self, loader):
        self.loader = loader

    def create_module(self, spec):
        return self.loader.create_module(spec)

    def exec_module(self, module):
        self.loader.exec_module(module)
        _observe_plot_managers(module)

    def __getattr__(self, name):
        return getattr(self.loader, name)


class _PlotImportFinder:
    def find_spec(self, fullname, path=None, target=None):
        if fullname != "matplotlib.backend_bases":
            return None
        # Delegate in the original finder order, including custom loaders;
        # never replace __import__ or eagerly import scientific packages.
        following = False
        for finder in tuple(sys.meta_path):
            if finder is self:
                following = True
            elif following:
                find_spec = getattr(finder, "find_spec", None)
                if find_spec is None:
                    continue
                spec = find_spec(fullname, path, target)
                if spec is not None:
                    if (spec.loader is not None
                            and hasattr(spec.loader, "create_module")
                            and hasattr(spec.loader, "exec_module")):
                        spec.loader = _PlotImportLoader(spec.loader)
                    return spec
        return None


_observe_plot_managers(sys.modules.get("matplotlib.backend_bases"))
sys.meta_path.insert(0, _PlotImportFinder())


class ToolBridgeError(RuntimeError):
    pass


def _bridge_owner():
    # A task can outlive its cell even in the same retained event loop. Its
    # ContextVars must not grant host I/O under the next cell's Rust lease.
    # Copied worker contexts may print, but cannot compete for protocol stdin.
    output = _OUTPUT_CONTEXT.get()
    if output is None or not output.active or not output.bridge_open:
        raise ToolBridgeError("EVAL_BRIDGE_CLOSED: tool call belongs to an inactive cell")
    if threading.get_ident() != output.thread:
        raise ToolBridgeError("EVAL_BRIDGE_THREAD: call tools on the owning cell's event-loop thread")
    if output.bridge_error is not None:
        raise ToolBridgeError(output.bridge_error)
    if _BRIDGE_FAILURE is not None:
        raise ToolBridgeError(_BRIDGE_FAILURE)
    return output


def _bridge_poison():
    global _BRIDGE_FAILURE
    _BRIDGE_FAILURE = (
        "EVAL_BRIDGE_PROTOCOL: host reply was incomplete or invalid; "
        "Python kernel is retiring. Reset Python before continuing; "
        "dispatched tool effects may already have occurred."
    )
    owner = _OUTPUT_CONTEXT.get()
    if owner is not None:
        owner.bridge_error = _BRIDGE_FAILURE
    return ToolBridgeError(_BRIDGE_FAILURE)


def _bridge_request(tool_name, tool_input):
    global _BRIDGE_CALL, _BRIDGE_ENCODING
    owner = _bridge_owner()
    if _BRIDGE_ENCODING:
        raise ToolBridgeError("EVAL_BRIDGE_BUSY: recursive tool argument serialization")
    if type(tool_name) is not str or not tool_name or type(tool_input) is not dict:
        raise TypeError("EVAL_BRIDGE_INPUT: expected a tool name and JSON-object arguments")
    if len(tool_name) > 128:
        raise ValueError("EVAL_BRIDGE_LIMIT: tool name exceeds 128 characters")
    # Preserve native primitive-only admission before any encoder callbacks.
    _display_json_shape(tool_input)
    _BRIDGE_ENCODING = True
    try:
        if _BRIDGE_CALL >= (1 << 64) - 1:
            raise ToolBridgeError("EVAL_BRIDGE_LIMIT: request identifiers exhausted; reset Python")
        call_id = _BRIDGE_CALL + 1
        frame = {"id": owner.cell_id, "bridge": {
            "call": call_id, "tool": tool_name, "input": tool_input,
        }}
        encoder = json.JSONEncoder(ensure_ascii=False, allow_nan=False, separators=(",", ":"))
        chunks, size = [], 1  # Include the terminating newline.
        for chunk in encoder.iterencode(frame):
            size += len(chunk.encode("utf-8"))
            if size > _MAX_BRIDGE_REQUEST_BYTES:
                raise ToolBridgeError("EVAL_BRIDGE_LIMIT: encoded request exceeds 1 MiB")
            chunks.append(chunk)
        # Snapshot before queueing: later guest mutation cannot retarget an
        # already awaited request. Rejected serialization never writes a frame.
        encoded = "".join(chunks) + "\n"
        _BRIDGE_CALL = call_id
        return call_id, encoded
    finally:
        _BRIDGE_ENCODING = False


def _bridge_send(encoded):
    try:
        with _PROTOCOL_WRITE_LOCK:
            _REAL_STDOUT.write(encoded)
            _REAL_STDOUT.flush()
    except BaseException:
        raise _bridge_poison() from None


def _bridge_read():
    try:
        # A partial line cannot masquerade as a reply after EOF. Count chars
        # before JSON parsing; one reply, not a reusable background reader.
        line = _REAL_STDIN.readline(_MAX_BRIDGE_REPLY_BYTES + 1)
        if not line.endswith("\n") or len(line.encode("utf-8")) > _MAX_BRIDGE_REPLY_BYTES:
            return None
    except BaseException:
        return None
    return line


def _bridge_result(line, call_id):
    try:
        reply = json.loads(line) if line is not None else None
    except BaseException:
        reply = None
    if type(reply) is not dict or set(reply) != {"bridge_result"}:
        raise _bridge_poison()
    result = reply.get("bridge_result") if type(reply) is dict else None
    if (type(result) is not dict or type(result.get("call")) is not int
            or result["call"] != call_id or type(result.get("ok")) is not bool):
        raise _bridge_poison()
    if result["ok"]:
        if type(result.get("content")) is not str:
            raise _bridge_poison()
        return result["content"]
    if type(result.get("error")) is not str:
        raise _bridge_poison()
    # A valid tool failure is not a framing failure and does not poison stdin.
    raise ToolBridgeError(result["error"])


def _bridge_call(tool_name, tool_input):
    global _BRIDGE_BUSY
    _bridge_owner()
    if _BRIDGE_BUSY or _BRIDGE_ENCODING or not _BRIDGE_EXCHANGE.acquire(blocking=False):
        raise ToolBridgeError("EVAL_BRIDGE_BUSY: another tool exchange already owns protocol stdin")
    # Guard serialization too: a dict subclass can execute guest code while
    # being encoded. Re-entry must fail before emitting a second request.
    _BRIDGE_BUSY = True
    try:
        call_id, encoded = _bridge_request(tool_name, tool_input)
        _bridge_send(encoded)
        return _bridge_result(_bridge_read(), call_id)
    finally:
        _BRIDGE_BUSY = False
        _BRIDGE_EXCHANGE.release()


async def _bridge_wait_reply(call_id):
    global _BRIDGE_READER
    received = queue.SimpleQueue()

    def read_once():
        received.put(_bridge_read())

    # One dedicated reader avoids blocking asyncio or depending on a guest's
    # saturated default executor. It never executes a tool or copies ContextVars.
    # Completion/cancellation drains and joins it before stdin can be reused.
    reader = threading.Thread(target=read_once, name="pi-eval-bridge", daemon=True)
    try:
        reader.start()
    except RuntimeError:
        raise _bridge_poison() from None
    _BRIDGE_READER = reader
    cancelled = None
    while True:
        try:
            line = received.get_nowait()
            break
        except queue.Empty:
            try:
                await asyncio.sleep(0.002)
            except asyncio.CancelledError as error:
                # Native delivery is uncertain. Finish this one exchange,
                # including under repeated cancellation, never send it again.
                # The Rust owner still enforces the original overall deadline.
                cancelled = error
    reader.join()
    _BRIDGE_READER = None
    try:
        result = _bridge_result(line, call_id)
    except ToolBridgeError:
        if cancelled is not None and _BRIDGE_FAILURE is None:
            raise cancelled
        raise
    if cancelled is not None:
        raise cancelled
    return result


async def _bridge_call_async(tool_name, tool_input):
    global _BRIDGE_BUSY
    owner = _bridge_owner()
    if len(owner.bridge_tasks) >= _MAX_BRIDGE_CALLS:
        raise ToolBridgeError("EVAL_BRIDGE_LIMIT: at most 64 active or queued tool calls per cell")
    task = asyncio.current_task()
    owner.bridge_tasks.add(task)
    try:
        call_id, encoded = _bridge_request(tool_name, tool_input)
        async with _BRIDGE_LOCK:
            # Queued calls must still belong to the open original cell at
            # dispatch. A cancelled waiter emits nothing and owns no reader.
            if _bridge_owner() is not owner:
                raise ToolBridgeError("EVAL_BRIDGE_CLOSED: tool owner changed while queued")
            if _BRIDGE_BUSY or not _BRIDGE_EXCHANGE.acquire(blocking=False):
                raise ToolBridgeError("EVAL_BRIDGE_BUSY: protocol stdin is already owned")
            _BRIDGE_BUSY = True
            try:
                _bridge_send(encoded)
                return await _bridge_wait_reply(call_id)
            finally:
                _BRIDGE_BUSY = False
                _BRIDGE_EXCHANGE.release()
    finally:
        owner.bridge_tasks.discard(task)


async def _finish_bridge_calls(output):
    # A fire-and-forget bridge cannot survive into main's next request read.
    # Close admission first, cancel waiters, and drain any dispatched exchange.
    output.bridge_open = False
    pending = tuple(output.bridge_tasks)
    if not pending:
        return None
    for task in pending:
        task.cancel()
    await asyncio.gather(*pending, return_exceptions=True)
    return "EVAL_BRIDGE_PENDING: cell ended with unawaited tool work; queued calls were cancelled and dispatched replies drained. Effects are not rolled back."


class _PiTools:
    """`tool` object exposed to cells: whitelisted re-entry into pi tools."""

    @staticmethod
    async def call(name, arguments):
        """Await a host tool with JSON-object arguments; host policy still applies.

        Other asyncio work advances while the host replies. Requests remain
        serialized; cancellation after dispatch waits for that reply to drain
        within the original host deadline. It never cancels or replays a tool.
        """
        if not isinstance(name, str) or not isinstance(arguments, dict):
            raise TypeError("tool.call requires a tool name and an argument object")
        return await _bridge_call_async(name, arguments)

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


def _pip_arguments(code):
    """Recognize one explicit install command; never interpret shell syntax."""
    if not isinstance(code, str):
        raise TypeError("cell code must be a string")
    command = code.strip()
    if not command or command.split(None, 1)[0] != "%pip":
        return None
    if "\n" in command or "\r" in command or "\x00" in command:
        raise ValueError("EVAL_PIP_USAGE: %pip install must be a standalone cell without NUL bytes")
    if len(command) > 16384 or len(command.encode("utf-8")) > 16384:
        raise ValueError("EVAL_PIP_LIMIT: install command exceeds 16 KiB")
    try:
        arguments = shlex.split(command)
    except ValueError as error:
        raise ValueError("EVAL_PIP_USAGE: quote requirements and paths correctly") from error
    if len(arguments) < 3 or arguments[:2] != ["%pip", "install"]:
        raise ValueError("EVAL_PIP_USAGE: %pip install <requirements/options>; other pip commands are not supported")
    if len(arguments) > 130 or any(not argument for argument in arguments[2:]):
        raise ValueError("EVAL_PIP_LIMIT: use at most 128 nonempty install arguments")
    return arguments[2:]


async def _forward_pip_output(reader, output):
    # Incremental decoding preserves code points split across pipe reads.
    # communicate() would buffer the entire installation log in the kernel.
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    while True:
        chunk = await reader.read(8192)
        if not chunk:
            break
        text = decoder.decode(chunk)
        if text:
            output.write(text)
    final = decoder.decode(b"", final=True)
    if final:
        output.write(final)


def _managed_pip_arguments(arguments):
    """Keep managed storage/interpreter policy out of caller-controlled flags."""
    flags = {"--no-index", "--no-deps", "--pre", "--upgrade"}
    valued = {"--find-links", "-f", "--index-url", "-i", "--extra-index-url"}
    result = []
    requirements = 0
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        key, separator, value = argument.partition("=")
        if argument in flags:
            result.append(argument)
        elif key in valued:
            if not separator:
                index += 1
                value = arguments[index] if index < len(arguments) else ""
            if not value or value.startswith("-"):
                raise ValueError("EVAL_PIP_USAGE: index/find-links options require a value")
            result.extend([key, value])
        elif not argument or argument.startswith("-"):
            raise ValueError("EVAL_PIP_OPTION: unsupported managed option; targets and interpreter are host-owned")
        else:
            result.append(argument)
            requirements += 1
        index += 1
    if not requirements:
        raise ValueError("EVAL_PIP_USAGE: managed installation requires at least one package")
    return result


async def _install_managed_packages(arguments):
    arguments = _managed_pip_arguments(arguments)
    if _PACKAGE_ROOT is None or not Path(_PACKAGE_ROOT).is_dir():
        raise RuntimeError("EVAL_PIP_UNAVAILABLE: no host-owned package directory")
    if len(_PACKAGE_LAYERS) >= _MAX_PACKAGE_LAYERS:
        raise ValueError("EVAL_PIP_LIMIT: at most 32 managed installs; reset the Python kernel")
    transaction = Path(tempfile.mkdtemp(prefix="install-", dir=_PACKAGE_ROOT))
    target = transaction / "site"
    activated = False
    try:
        scratch = transaction / "tmp"
        scratch.mkdir()
        environment = {key: value for key, value in os.environ.items()
                       if not key.upper().startswith("PIP_")}
        environment.update({"PIP_CONFIG_FILE": os.devnull,
                            "TMPDIR": str(scratch), "TEMP": str(scratch), "TMP": str(scratch)})
        print("EVAL_PIP_INSTALLING: staging isolated kernel-local packages")
        # Use the same concurrent output pumps, interpreter and cancellation
        # path as ordinary pip. Never pause/extend the host's cell watchdog.
        await _install_python_packages([
            "--ignore-installed", "--only-binary=:all:", "--no-compile",
            "--no-cache-dir", "--progress-bar", "off", "--target", str(target),
            *arguments,
        ], environment=environment)
        if not target.is_dir() or next(target.iterdir(), None) is None:
            print("EVAL_PACKAGES_UNCHANGED: pip selected no packages for this interpreter")
            return
        # No addsitedir/.pth hooks and no automatic reload of live modules.
        # Every dependency generation is complete before it enters sys.path.
        sys.path.insert(0, str(target))
        _PACKAGE_LAYERS.append(str(target))
        activated = True
        print("EVAL_PACKAGES_READY: active until Python reset; existing imports remain cached")
    finally:
        if not activated:
            shutil.rmtree(transaction)


async def _install_python_packages(arguments, *, environment=None):
    if arguments and arguments[0] == "--managed":
        return await _install_managed_packages(arguments[1:])
    # -I prevents a workspace pip.py/PYTHONPATH from replacing the installer.
    # Pip itself retains its usual configuration and requested install options;
    # this is not a sandbox, lockfile resolver or automatic environment manager.
    environment = (os.environ if environment is None else environment).copy()
    environment["PYTHONIOENCODING"] = "utf-8:replace"
    environment["PYTHONUNBUFFERED"] = "1"
    print("EVAL_PIP_START: running pip with the retained Python interpreter", flush=True)
    try:
        process = await asyncio.create_subprocess_exec(
            _KERNEL_PYTHON, "-I", "-u", "-X", "utf8", "-m", "pip", "--disable-pip-version-check",
            "--no-input", "--no-color", "install", *arguments,
            cwd=str(_WORKSPACE_ROOT), env=environment,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            limit=65536,
        )
    except (OSError, NotImplementedError) as error:
        raise RuntimeError("EVAL_PIP_START_FAILED: unable to start this interpreter's pip") from error
    # The child inherits the kernel's process group/Windows Job. The Rust
    # owner retires that tree on timeout, cancellation, or session shutdown.
    readers = []
    try:
        readers.append(asyncio.create_task(_forward_pip_output(process.stdout, sys.stdout)))
        readers.append(asyncio.create_task(_forward_pip_output(process.stderr, sys.stderr)))
        await asyncio.gather(*readers)
        status = await process.wait()
    except BaseException:
        # Also own cleanup for a Python task cancellation/output failure. Do
        # not retain pump tasks that could resume against another cell's sink.
        if process.returncode is None:
            try:
                process.kill()
            except ProcessLookupError:
                pass
        for reader in readers:
            reader.cancel()
        await asyncio.gather(*readers, return_exceptions=True)
        await process.wait()
        raise
    finally:
        # Failure may have installed some packages. Never promise rollback,
        # replay .pth hooks, or discard/reload the caller's imported modules.
        importlib.invalidate_caches()
    if status != 0:
        raise RuntimeError(
            f"EVAL_PIP_FAILED: pip exited with code {status}; see installer output. "
            "If pip is missing, provision it for PI_EVAL_PYTHON; no bootstrap was run. "
            "Package changes already made are not rolled back."
        )
    print("EVAL_PIP_DONE: pip completed; import caches refreshed, existing modules retained", flush=True)


async def _evaluate_cell(code, displays):
    install = _pip_arguments(code)
    if install is not None:
        await _install_python_packages(install)
        return None
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
        return None
    value = eval(expression, NAMESPACE)  # noqa: S307
    # Only await compiler-generated top-level-await code, not an arbitrary
    # awaitable that happens to be the final value (normal REPL semantics).
    if expression.co_flags & inspect.CO_COROUTINE:
        value = await value
    if value is not None and displays.append_result(value):
        return None
    return _result_text(value) if value is not None else None


async def _execute_cell(code, displays, output):
    global _CELL_CONTEXT
    display_token = _DISPLAY_CONTEXT.set(displays)
    output_token = _OUTPUT_CONTEXT.set(output)
    result = error = None
    try:
        try:
            result = await _evaluate_cell(code, displays)
        except ModuleNotFoundError:
            error = _error_text() + (
                "\nEVAL_PACKAGE_HINT: use a separate `%pip install <distribution>` cell "
                "for this interpreter. Distribution and import names may differ "
                "(for example, pillow provides PIL). Nothing was installed automatically.\n"
            )
        except BaseException:  # noqa: BLE001 - SystemExit must not stop the loop
            error = _error_text()
        bridge_error = await _finish_bridge_calls(output)
        if bridge_error is not None:
            error = (error or "") + "\n" + bridge_error
        # This still runs under the creating cell's output identity and host
        # deadline. A failed body retains its plot output, but no render error
        # can turn the original failure into success or erase its diagnostic.
        try:
            plot_error = displays.finish_plots()
        except BaseException:
            plot_error = _error_text()
        if plot_error is not None:
            error = (error or "") + "\nEVAL_PLOT_OUTPUT: plot finalization failed\n" + plot_error
        return error is None, result if error is None else None, error
    finally:
        # Child tasks retain this particular sink. Once the cell ends they
        # cannot publish late output into a later cell's display collection.
        output.bridge_open = False
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
        if _BRIDGE_BUSY or _BRIDGE_READER is not None:
            # An unexpected event-loop stop can bypass coroutine cleanup.
            # Drain the sole reader before retiring this process, never let
            # main compete with it or exit while it holds a buffered-I/O lock.
            _bridge_poison()
            if _BRIDGE_READER is not None:
                _BRIDGE_READER.join()
        displays.active = False
        with _PROTOCOL_WRITE_LOCK:
            output.active = False
            sequence = output.sequence
        sys.stdout, sys.stderr = old_out, old_err
    if _BRIDGE_FAILURE is not None:
        ok, result, error = False, None, (error or "") + "\n" + _BRIDGE_FAILURE
    return ok, result, error, displays.frames, sequence, output.bridge_error or _BRIDGE_FAILURE


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
            ok, result, error, displays, sequence, bridge_error = run_cell(request.get("code", ""), request.get("id"))
            response = {
                "id": request.get("id"),
                "ok": ok,
                "stdout": "",
                "stderr": "",
                "displays": displays,
                "outputSequence": sequence,
            }
            if bridge_error is not None:
                response["bridgeError"] = bridge_error
            if ok:
                response["result"] = result
            else:
                response["error"] = error
        except Exception as exc:  # noqa: BLE001 - protocol-level failure
            response = {"id": None, "ok": False, "stdout": "", "stderr": "", "error": str(exc)}
        _send_frame(response)
        if _BRIDGE_FAILURE is not None:
            # Never reinterpret remaining reply bytes as the next cell.
            break


if __name__ == "__main__":
    main()
