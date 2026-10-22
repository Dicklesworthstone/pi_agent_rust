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
Use `await tool.result("read", {"path": path})` to preserve native content,
details, and isError instead of flattened text. This uses the same bridge and
does not enable any additional tools. Native tool errors are returned data;
bridge, transport, and result-decoding failures raise ToolBridgeError.
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
import signal
import stat
import sys
import tempfile
import threading
import time
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
_SOFT_DEADLINE = None
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
        self.soft_timed_out = False
        self.kernel_error = None
        self.package_operation = False
        self.bridge_interrupted = False
        self.finalized = False
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
    if output.soft_timed_out:
        raise ToolBridgeError("EVAL_SOFT_TIMEOUT: host tool calls are revoked after the soft deadline")
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
    async def result(name, arguments):
        """Await native content/details/isError under the ordinary cell owner.

        No separate reader, dispatcher, or retry path: cancellation still drains
        an already dispatched reply before releasing the shared protocol lock.
        Decoding happens only after that exchange completes. A malformed inner
        envelope cannot leave reply bytes for another call or cell to consume.
        """
        if (type(name) is not str or not name or len(name) > 128
                or len(name.encode("utf-8")) > 128 or type(arguments) is not dict):
            raise TypeError("EVAL_BRIDGE_INPUT: tool.result requires a tool name of at most 128 UTF-8 bytes and an argument object")
        encoded = await _bridge_call_async("__pi_tool_result", {"name": name, "args": arguments})
        # The native serializer has the same bound. Check before decoding and
        # count UTF-8 bytes, not just Python characters. No successful prefix.
        limit = 4 * 1024 * 1024
        if len(encoded) > limit or len(encoded.encode("utf-8")) > limit:
            raise ToolBridgeError("EVAL_BRIDGE_LIMIT: structured result exceeds 4 MiB")

        def reject_constant(_value):
            # NaN/Infinity are accepted by Python's default decoder, but cannot
            # occur in the native JSON contract (or JavaScript's JSON.parse).
            raise ValueError("non-JSON number")

        try:
            result = json.loads(encoded, parse_constant=reject_constant)
        except (ValueError, RecursionError):
            raise ToolBridgeError("EVAL_BRIDGE_PROTOCOL: invalid structured tool result") from None
        if (type(result) is not dict or result.get("schema") != "pi.eval.tool_result.v1"
                or type(result.get("content")) is not list or "details" not in result
                or type(result.get("isError")) is not bool):
            raise ToolBridgeError("EVAL_BRIDGE_PROTOCOL: invalid structured tool result")
        return result

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


def _agent_wait_ms(timeout):
    if type(timeout) not in (int, float) or not math.isfinite(timeout) or not 0 <= timeout <= 600:
        raise ValueError("EVAL_AGENT_INPUT: timeout must be finite seconds between 0 and 600")
    return int(timeout * 1000)


def _agent_result_text(result):
    return "\n".join(block.get("text", "") for block in result["content"]
                     if type(block) is dict and block.get("type") == "text")


async def _agent_native_result(name, arguments):
    result = await _PiTools.result(name, arguments)
    if result["isError"]:
        raise ToolBridgeError(_agent_result_text(result) or "Agent job operation failed")
    return result["details"]


async def _agent_read_job(job_id, action, timeout_ms=None):
    arguments = {"action": action, "jobId": job_id}
    if timeout_ms is not None:
        arguments["timeoutMs"] = timeout_ms
    snapshot = await _agent_native_result("jobs", arguments)
    if (type(snapshot) is not dict or snapshot.get("schema") != "pi.host_job.v1"
            or snapshot.get("id") != job_id or type(snapshot.get("status")) is not str):
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: invalid native job snapshot")
    return snapshot


async def _agent_wait_one(job, timeout_ms, raise_errors):
    if type(job) is _AgentTurn:
        return await _agent_wait_turn(job, timeout_ms, raise_errors)
    if type(job) is _AgentPoolItem:
        return await _pool_wait_item(job, timeout_ms, raise_errors)
    if type(job) is _ModelJob:
        return await _model_wait_job(job, timeout_ms, raise_errors)
    snapshot = await _agent_read_job(job.id, "wait", timeout_ms)
    if snapshot["status"] == "running":
        raise TimeoutError("EVAL_AGENT_TIMEOUT: job is still running; it remains available through jobs")
    result = snapshot.get("result")
    if (type(result) is not dict or type(result.get("content")) is not list
            or "details" not in result or type(result.get("isError", False)) is not bool):
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: settled job has no native result")
    if raise_errors and (snapshot["status"] != "exited" or result.get("isError", False)):
        raise ToolBridgeError(_agent_result_text(result) or f"Agent job {snapshot['status']}")
    return result


class _AgentJob:
    """An opaque native job id; every operation uses the active cell's host."""

    __slots__ = ("_id", "agent")

    def __init__(self, job_id, selected_agent):
        self._id = job_id
        self.agent = selected_agent

    @property
    def id(self):
        return self._id

    async def status(self):
        return await _agent_read_job(self.id, "wait", 0)

    async def done(self):
        return (await self.status())["status"] != "running"

    async def output(self):
        return (await self.status()).get("result")

    async def cancel(self):
        return await _agent_read_job(self.id, "cancel")

    async def wait(self, timeout=30):
        return await _agent_wait_one(self, _agent_wait_ms(timeout), True)

    def __await__(self):
        return self.wait().__await__()


def _model_job_id(value):
    return (type(value) is str and 0 < len(value) <= 128
            and not any(character.isspace() or ord(character) < 32 or ord(character) == 127
                        for character in value))


def _model_protocol():
    return ToolBridgeError("EVAL_MODEL_PROTOCOL: invalid native model job response")


def _model_payload(operation, payload):
    if type(payload) is not dict:
        raise _model_protocol()
    if operation == "completion":
        if (payload.get("schema") != "pi.completion.v1" or type(payload.get("text")) is not str
                or ("data" in payload and payload.get("schemaValid") is not True)
                or ("schemaValid" in payload and ("data" not in payload or payload["schemaValid"] is not True))):
            raise _model_protocol()
    elif operation == "judgment":
        if (payload.get("schema") != "pi.judgment.v1" or type(payload.get("answers")) is not dict
                or not payload["answers"]):
            raise _model_protocol()
    else:
        if (payload.get("schema") != "pi.eval.judgment-batch.v1" or type(payload.get("items")) is not list
                or type(payload.get("total")) is not int or payload["total"] != len(payload["items"])
                or payload["total"] < 1
                or type(payload.get("succeeded")) is not int or payload["succeeded"] < 0
                or type(payload.get("failed")) is not int or payload["failed"] < 0
                or payload["succeeded"] + payload["failed"] != payload["total"]
                or payload.get("allOk") is not (payload["failed"] == 0)):
            raise _model_protocol()
        succeeded = 0
        for item in payload["items"]:
            if (type(item) is not dict or type(item.get("ok")) is not bool
                    or not (type(item.get("key")) is str and bool(item["key"])
                            or type(item.get("key")) is int and item["key"] >= 0)):
                raise _model_protocol()
            if item["ok"]:
                _model_payload("judgment", item.get("verdict"))
                succeeded += 1
            elif type(item.get("error")) is not str or not item["error"]:
                raise _model_protocol()
        if succeeded != payload["succeeded"]:
            raise _model_protocol()
    return payload


def _model_check_progress(job, result, wrapper_schema):
    if (job.operation != "judgment_batch" or type(result) is not dict or type(result.get("content")) is not list
            or type(result.get("isError", False)) is not bool
            or type(result.get("details")) is not dict or result["details"].get("schema") != wrapper_schema):
        raise _model_protocol()
    partial = result["details"].get("partialResult")
    if (type(partial) is not dict or type(partial.get("content")) is not list
            or partial.get("isError", False) is not False or type(partial.get("details")) is not dict):
        raise _model_protocol()
    progress = partial["details"]
    if (progress.get("schema") != "pi.eval.model-job.progress.v1" or progress.get("operation") != "judgment_batch"
            or progress.get("complete") is not False or type(progress.get("total")) is not int
            or not 1 <= progress["total"] <= 16
            or type(progress.get("items")) is not list or len(progress["items"]) != progress["total"]
            or any(type(progress.get(name)) is not int or not 0 <= progress[name] <= progress["total"]
                   for name in ("completed", "succeeded", "failed", "queued", "running"))
            or progress["completed"] != progress["succeeded"] + progress["failed"]
            or progress["total"] != progress["completed"] + progress["queued"] + progress["running"]
            or type(progress.get("judgments")) is not dict
            or progress["judgments"].get("schema") != "pi.eval.judgments.v1"
            or type(progress["judgments"].get("attempts")) is not list
            or len(progress["judgments"]["attempts"]) != progress["total"]
            or any(type(receipt) is not dict for receipt in progress["judgments"]["attempts"])):
        raise _model_protocol()
    counts = {"queued": 0, "running": 0, "completed": 0, "failed": 0}
    seen = set()
    for item in progress["items"]:
        if (type(item) is not dict or type(item.get("status")) is not str or item["status"] not in counts
                or not (type(item.get("key")) is str and bool(item["key"])
                        or type(item.get("key")) is int and item["key"] >= 0)):
            raise _model_protocol()
        identity = (type(item["key"]), item["key"])
        if identity in seen:
            raise _model_protocol()
        seen.add(identity)
        counts[item["status"]] += 1
        if item["status"] == "completed":
            if "error" in item:
                raise _model_protocol()
            _model_payload("judgment", item.get("verdict"))
        elif item["status"] == "failed":
            if "verdict" in item or type(item.get("error")) is not str or not item["error"]:
                raise _model_protocol()
        elif "verdict" in item or "error" in item:
            raise _model_protocol()
    if (counts["queued"] != progress["queued"] or counts["running"] != progress["running"]
            or counts["completed"] != progress["succeeded"] or counts["failed"] != progress["failed"]):
        raise _model_protocol()
    return progress


def _model_progress_view(job, snapshot):
    result = snapshot.get("result")
    details = result.get("details") if result is not None else None
    if details is None:
        return None
    if details["schema"] in ("pi.host_job.progress.v1", "pi.host_job.interrupted.v1"):
        return {**details["partialResult"]["details"], "jobStatus": snapshot["status"]}
    batch = details["result"]
    if batch is None:
        return None
    return {"schema": "pi.eval.model-job.progress.v1", "operation": job.operation, "complete": True,
            "jobStatus": snapshot["status"], "total": batch["total"], "completed": batch["total"],
            "succeeded": batch["succeeded"], "failed": batch["failed"], "queued": 0, "running": 0,
            "items": [{"key": item["key"], "status": "completed", "verdict": item["verdict"]} if item["ok"]
                      else {"key": item["key"], "status": "failed", "error": item["error"]} for item in batch["items"]],
            "judgments": details["judgments"]}


def _model_check_job(job, snapshot):
    expected_label = "eval model completion" if job.operation == "completion" else f"eval {job.operation}"
    if (snapshot.get("command") != expected_label
            or snapshot["status"] not in ("running", "exited", "failed", "killed", "timedOut")):
        raise _model_protocol()
    if snapshot["status"] == "running":
        if snapshot.get("result") is not None:
            _model_check_progress(job, snapshot["result"], "pi.host_job.progress.v1")
            if snapshot["result"].get("isError", False):
                raise _model_protocol()
        return snapshot
    result = snapshot.get("result")
    if (type(result) is not dict or type(result.get("content")) is not list or "details" not in result
            or type(result.get("isError", False)) is not bool):
        raise _model_protocol()
    details = result["details"]
    if details is None:
        # Cancellation/timeout may retire native work before any typed answer.
        # Keep the real failure, never turn absent evidence into a verdict.
        if snapshot["status"] == "exited" or not result.get("isError", False):
            raise _model_protocol()
    elif type(details) is dict and details.get("schema") == "pi.host_job.interrupted.v1":
        if snapshot["status"] not in ("killed", "timedOut") or not result.get("isError", False):
            raise _model_protocol()
        _model_check_progress(job, result, "pi.host_job.interrupted.v1")
    else:
        if (type(details) is not dict or details.get("schema") != "pi.eval.model-job.result.v1"
                or details.get("operation") != job.operation or "result" not in details
                or type(details.get("judgments")) is not dict
                or details["judgments"].get("schema") != "pi.eval.judgments.v1"
                or type(details["judgments"].get("attempts")) is not list):
            raise _model_protocol()
        if details["result"] is None:
            if not result.get("isError", False) or type(details.get("error")) is not str or not details["error"]:
                raise _model_protocol()
        else:
            payload = _model_payload(job.operation, details["result"])
            if job.operation == "judgment_batch" and result.get("isError", False) is not (not payload["allOk"]):
                raise _model_protocol()
            if job.operation != "judgment_batch" and result.get("isError", False):
                raise _model_protocol()
        if snapshot["status"] != "exited" and not result.get("isError", False):
            raise _model_protocol()
    return snapshot


async def _model_read_job(job, action, timeout_ms=None):
    return _model_check_job(job, await _agent_read_job(job.id, action, timeout_ms))


async def _model_wait_job(job, timeout_ms, raise_errors):
    snapshot = await _model_read_job(job, "wait", timeout_ms)
    if snapshot["status"] == "running":
        raise TimeoutError("EVAL_MODEL_TIMEOUT: model job is still running; retain its id and wait again")
    result = snapshot["result"]
    if raise_errors and (snapshot["status"] != "exited" or result.get("isError", False)):
        error = ToolBridgeError(_agent_result_text(result) or f"Model job {snapshot['status']}")
        error.result = result
        error.job_id = job.id
        raise error
    details = result["details"]
    payload = (details["result"] if details is not None
               and details.get("schema") == "pi.eval.model-job.result.v1" else None)
    if payload is None:
        return result
    if job.operation == "completion":
        return payload["data"] if "data" in payload else payload["text"]
    return payload["answers"] if job.operation == "judgment" else payload


def _model_drain_input():
    return TypeError("EVAL_BATCH_INPUT: drain accepts cursor and timeout seconds from 0 through 600")


def _model_drain_protocol():
    return ToolBridgeError("EVAL_BATCH_PROTOCOL: invalid native drain response")


def _model_drain_cursor(value, job_id, fail):
    if value is None:
        return None
    if (type(value) is not dict or any(type(name) is not str for name in value)
            or set(value) != {"schema", "jobId", "manifest", "seen"}
            or type(value["schema"]) is not str or value["schema"] != "pi.eval.batch-cursor.v1"
            or type(value["jobId"]) is not str or value["jobId"] != job_id
            or type(value["seen"]) is not list or len(value["seen"]) > 16):
        raise fail()
    manifest = value["manifest"]
    if manifest is not None and (type(manifest) is not str or len(manifest) != 64
                                or any(character not in "0123456789abcdef" for character in manifest)):
        raise fail()
    seen = value["seen"]
    if (any(type(index) is not int or not 0 <= index < 16 for index in seen)
            or len(set(seen)) != len(seen) or seen and manifest is None):
        raise fail()
    # Copy before awaiting. A caller retains its acknowledgment and cannot
    # mutate either the queued request or the validation baseline in flight.
    return {"schema": value["schema"], "jobId": job_id, "manifest": manifest, "seen": sorted(seen)}


def _model_check_drain(page, job_id, previous):
    if (type(page) is not dict or page.get("schema") != "pi.eval.batch-drain.v1"
            or page.get("jobId") != job_id
            or page.get("jobStatus") not in ("running", "exited", "failed", "killed", "timedOut")
            or page.get("done") is not (page["jobStatus"] != "running")
            or page.get("jobFailed") is not (page["jobStatus"] not in ("running", "exited"))
            or type(page.get("complete")) is not bool or type(page.get("evidenceAvailable")) is not bool
            or type(page.get("items")) is not list or len(page["items"]) > 16):
        raise _model_drain_protocol()
    cursor = _model_drain_cursor(page.get("cursor"), job_id, _model_drain_protocol)
    if cursor is None:
        raise _model_drain_protocol()
    before = set(previous["seen"]) if previous is not None else set()
    manifest = previous["manifest"] if previous is not None else None
    if manifest is not None and cursor["manifest"] != manifest:
        raise _model_drain_protocol()
    if not page["evidenceAvailable"]:
        if ("total" not in page or page["total"] is not None
                or "settled" not in page or page["settled"] is not None
                or page["complete"] or page["items"] or page["jobStatus"] == "exited"
                or cursor["manifest"] != manifest or set(cursor["seen"]) != before):
            raise _model_drain_protocol()
        return page
    if (type(page.get("total")) is not int or not 1 <= page["total"] <= 16
            or type(page.get("settled")) is not int or not 0 <= page["settled"] <= page["total"]
            or cursor["manifest"] is None
            or (page["complete"] and (not page["done"] or page["settled"] != page["total"]
                                      or page["jobStatus"] not in ("exited", "failed")))
            or (not page["complete"] and page["jobStatus"] not in ("running", "killed", "timedOut"))
            or any(index >= page["total"] for index in cursor["seen"])):
        raise _model_drain_protocol()
    added, keys = set(), set()
    last_index, key_type = -1, None
    for item in page["items"]:
        if (type(item) is not dict or type(item.get("index")) is not int
                or not last_index < item["index"] < page["total"] or item["index"] in before
                or type(item.get("ok")) is not bool
                or not (type(item.get("key")) is str and 0 < len(item["key"]) <= 128
                        or type(item.get("key")) is int and item["key"] == item["index"])
                or item["key"] in keys or key_type is not None and type(item["key"]) is not key_type):
            raise _model_drain_protocol()
        last_index, key_type = item["index"], type(item["key"])
        keys.add(item["key"])
        added.add(item["index"])
        if item["ok"]:
            if "error" in item:
                raise _model_drain_protocol()
            try:
                _model_payload("judgment", item.get("verdict"))
            except ToolBridgeError:
                raise _model_drain_protocol() from None
        elif ("verdict" in item or type(item.get("error")) is not str or not item["error"]
              or page["jobStatus"] == "exited"):
            raise _model_drain_protocol()
    if len(before) + len(added) != page["settled"] or set(cursor["seen"]) != before | added:
        raise _model_drain_protocol()
    return page


async def _model_drain_job(job, cursor, timeout):
    if (job.operation != "judgment_batch" or not _model_job_id(job.id)
            or type(timeout) not in (int, float) or not 0 <= timeout <= 600 or not math.isfinite(timeout)):
        raise _model_drain_input()
    cursor = _model_drain_cursor(cursor, job.id, _model_drain_input)
    # The native waiter owns readiness, policy rechecks and deadline accounting.
    # A drain never acknowledges locally, polls jobs, retries or cancels the job.
    encoded = await _bridge_call_async("__pi_judge_batch_drain", {
        "jobId": job.id, "cursor": cursor, "timeoutMs": int(timeout * 1000),
    })
    try:
        page = json.loads(encoded)
    except (ValueError, RecursionError):
        raise _model_drain_protocol() from None
    return _model_check_drain(page, job.id, cursor)


class _ModelJob:
    """Native model identity; every read and cancellation uses the current host."""

    __slots__ = ("_id", "_operation")

    def __init__(self, job_id, operation):
        self._id = job_id
        self._operation = operation

    @property
    def id(self):
        return self._id

    @property
    def job_id(self):
        return self._id

    @property
    def operation(self):
        return self._operation

    async def status(self):
        return await _model_read_job(self, "wait", 0)

    async def done(self):
        return (await self.status())["status"] != "running"

    async def output(self):
        return (await self.status()).get("result")

    async def progress(self):
        if self.operation != "judgment_batch":
            raise TypeError("EVAL_MODEL_INPUT: progress is available only for judgment batches")
        return _model_progress_view(self, await self.status())

    async def drain(self, *, cursor=None, timeout=30):
        """Read new batch items; save the returned cursor only after processing."""
        return await _model_drain_job(self, cursor, timeout)

    async def cancel(self):
        return await _model_read_job(self, "cancel")

    async def wait(self, timeout=30, raise_errors=True):
        if type(raise_errors) is not bool:
            raise TypeError("EVAL_MODEL_INPUT: raise_errors must be boolean")
        return await _model_wait_job(self, _agent_wait_ms(timeout), raise_errors)

    def __await__(self):
        return self.wait().__await__()


def _model_options(options, allowed):
    arguments = {}
    for name, value in options.items():
        if name not in allowed:
            raise TypeError(f"EVAL_MODEL_INPUT: unknown option {name}")
        if name == "model" and (type(value) is not str or value not in (
                "default", "smol", "slow", "plan", "commit", "vision", "designer", "task", "advisor", "tiny")):
            raise TypeError("EVAL_MODEL_INPUT: model must name a canonical native role")
        if name == "timeout_seconds" and (type(value) is not int or not 1 <= value <= 600):
            raise ValueError("EVAL_MODEL_INPUT: timeout_seconds must be an integer from 1 to 600")
        if name == "max_tokens" and (type(value) is not int or not 1 <= value <= 4096):
            raise ValueError("EVAL_MODEL_INPUT: max_tokens must be an integer from 1 to 4096")
        if name == "concurrency" and (type(value) is not int or not 1 <= value <= 8):
            raise ValueError("EVAL_MODEL_INPUT: concurrency must be an integer from 1 to 8")
        arguments[allowed[name]] = value
    return arguments


async def _model_launch(operation, arguments):
    bridge = {"completion": "__pi_model_complete_start", "judgment": "__pi_judge_start",
              "judgment_batch": "__pi_judge_batch_start"}[operation]
    encoded = await _bridge_call_async(bridge, arguments)
    try:
        started = json.loads(encoded)
    except (ValueError, RecursionError):
        raise _model_protocol() from None
    expected_timeout = arguments.get("timeout_secs", 300 if operation == "judgment_batch" else 60)
    if (type(started) is not dict or started.get("schema") != "pi.eval.model-job.v1"
            or started.get("operation") != operation or not _model_job_id(started.get("jobId"))
            or type(started.get("timeoutSeconds")) is not int or started["timeoutSeconds"] != expected_timeout):
        raise _model_protocol()
    # A lost or malformed acknowledgment can follow native admission. Never
    # replay automatically; recover the job id through jobs and attach to it.
    return _ModelJob(started["jobId"], operation)


async def _completion(prompt, **options):
    if type(prompt) is not str or not prompt.strip():
        raise TypeError("EVAL_MODEL_INPUT: completion requires a nonempty prompt")
    arguments = _model_options(options, {"model": "model", "system": "system", "schema": "schema",
                               "images": "images", "max_tokens": "max_tokens", "timeout_seconds": "timeout_secs"})
    return await _model_launch("completion", {"prompt": prompt, **arguments})


async def _judge(state, questions, **options):
    if type(questions) is not dict or not questions:
        raise TypeError("EVAL_MODEL_INPUT: questions must be a nonempty object")
    return await _model_launch("judgment", {"state": state, "questions": questions,
                              **_model_options(options, {"model": "model", "images": "images", "timeout_seconds": "timeout_secs"})})


async def _judge_batch(states, questions, **options):
    if type(states) not in (list, dict) or not states or type(questions) is not dict or not questions:
        raise TypeError("EVAL_MODEL_INPUT: judge_batch requires nonempty states and questions")
    return await _model_launch("judgment_batch", {"states": states, "questions": questions,
                              **_model_options(options, {"model": "model", "images": "images", "concurrency": "concurrency",
                                                        "timeout_seconds": "timeout_secs"})})


def _model_attachment(operation):
    async def attach(job_id):
        if not _model_job_id(job_id):
            raise TypeError("EVAL_MODEL_INPUT: attach requires a bounded native job id")
        job = _ModelJob(job_id, operation)
        await job.status()
        return job
    return attach


_completion.attach = _model_attachment("completion")
_judge.attach = _model_attachment("judgment")
_judge_batch.attach = _model_attachment("judgment_batch")


def _agent_worker_snapshot(snapshot, worker_id=None, turn_id=None):
    if (type(snapshot) is not dict or snapshot.get("schema") != "pi.subagent.worker.v1"
            or type(snapshot.get("workerId")) is not str or not snapshot["workerId"]
            or (worker_id is not None and snapshot["workerId"] != worker_id)
            or type(snapshot.get("jobId")) is not str or not snapshot["jobId"]
            or type(snapshot.get("agent")) is not str or not snapshot["agent"]
            or snapshot.get("status") not in ("starting", "running", "idle", "closed", "failed")
            or type(snapshot.get("turnId")) is not int or not 1 <= snapshot["turnId"] <= 32
            or (turn_id is not None and snapshot["turnId"] != turn_id)):
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: invalid native worker snapshot")
    turn = snapshot.get("turn")
    if (type(turn) is not dict or type(turn.get("id")) is not int
            or turn["id"] != snapshot["turnId"] or "result" not in turn
            or turn.get("status") not in ("queued", "running", "completed", "failed", "cancelled")):
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: invalid native worker snapshot")
    result = turn["result"]
    if result is None and turn["status"] not in ("queued", "running"):
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: settled worker turn has no native result")
    if result is not None and (type(result) is not dict or type(result.get("content")) is not list
                              or "details" not in result or type(result.get("isError", False)) is not bool):
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: invalid native worker result")
    return snapshot


async def _agent_read_worker(turn, operation, timeout_ms=None):
    arguments = {"operation": operation, "workerId": turn.worker_id}
    if operation != "worker_close":
        arguments["turnId"] = turn.turn_id
    if timeout_ms is not None:
        arguments["timeoutMs"] = timeout_ms
    snapshot = _agent_worker_snapshot(await _agent_native_result("subagent", arguments),
                                     turn.worker_id, None if operation == "worker_close" else turn.turn_id)
    if snapshot["jobId"] != turn.job_id or snapshot["agent"] != turn.agent:
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: native worker identity changed")
    return snapshot


def _agent_turn_done(turn):
    return turn["status"] not in ("queued", "running")


async def _agent_wait_turn(turn, timeout_ms, raise_errors):
    snapshot = await _agent_read_worker(turn, "worker_wait", timeout_ms)
    if not _agent_turn_done(snapshot["turn"]):
        raise TimeoutError("EVAL_AGENT_TIMEOUT: worker turn is still pending; the worker remains available")
    result = snapshot["turn"]["result"]
    if result is None:
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: settled worker turn has no native result")
    if raise_errors and (snapshot["turn"]["status"] != "completed" or result.get("isError", False)):
        raise ToolBridgeError(_agent_result_text(result) or f"Agent turn {snapshot['turn']['status']}")
    return result


class _AgentTurn:
    """A fixed turn in a native worker. Cancellation closes its whole worker."""

    __slots__ = ("_worker_id", "_turn_id", "_job_id", "_agent")

    def __init__(self, worker, turn_id):
        self._worker_id = worker.id
        self._turn_id = turn_id
        self._job_id = worker.job_id
        self._agent = worker.agent

    @property
    def id(self):
        return self._turn_id

    @property
    def turn_id(self):
        return self._turn_id

    @property
    def worker_id(self):
        return self._worker_id

    @property
    def job_id(self):
        return self._job_id

    @property
    def agent(self):
        return self._agent

    async def status(self):
        return await _agent_read_worker(self, "worker_status")

    async def done(self):
        return _agent_turn_done((await self.status())["turn"])

    async def output(self):
        return (await self.status())["turn"]["result"]

    async def wait(self, timeout=30):
        return await _agent_wait_turn(self, _agent_wait_ms(timeout), True)

    async def close(self):
        return await _agent_read_worker(self, "worker_close")

    async def cancel(self):
        return await self.close()

    def __await__(self):
        return self.wait().__await__()


class _AgentWorker:
    """A retained native child session; send returns a separately waitable turn."""

    __slots__ = ("_id", "_job_id", "_agent", "_turn_id")

    def __init__(self, snapshot):
        self._id = snapshot["workerId"]
        self._job_id = snapshot["jobId"]
        self._agent = snapshot["agent"]
        self._turn_id = snapshot["turnId"]

    @property
    def id(self):
        return self._id

    @property
    def worker_id(self):
        return self._id

    @property
    def job_id(self):
        return self._job_id

    @property
    def agent(self):
        return self._agent

    @property
    def turn_id(self):
        return self._turn_id

    async def send(self, prompt):
        if type(prompt) is not str or not prompt.strip():
            raise TypeError("EVAL_AGENT_INPUT: send requires a nonempty prompt")
        previous = self._turn_id
        snapshot = _agent_worker_snapshot(await _agent_native_result("subagent", {
            "operation": "worker_send", "workerId": self.id, "task": prompt,
        }), self.id)
        if (snapshot["jobId"] != self.job_id or snapshot["agent"] != self.agent
                or snapshot["turnId"] <= previous):
            raise ToolBridgeError("EVAL_AGENT_PROTOCOL: invalid worker send acknowledgment")
        # Responses to concurrent sends may arrive out of order. Keep the
        # newest native selection without changing earlier fixed turn handles.
        self._turn_id = max(self._turn_id, snapshot["turnId"])
        return _AgentTurn(self, snapshot["turnId"])

    async def status(self):
        return await _AgentTurn(self, self._turn_id).status()

    async def done(self):
        return await _AgentTurn(self, self._turn_id).done()

    async def output(self):
        return await _AgentTurn(self, self._turn_id).output()

    async def wait(self, timeout=30):
        return await _AgentTurn(self, self._turn_id).wait(timeout)

    async def close(self):
        return await _AgentTurn(self, self._turn_id).close()

    async def cancel(self):
        return await self.close()

    def __await__(self):
        return self.wait().__await__()


async def _agent(prompt, **options):
    if type(prompt) is not str or not prompt.strip():
        raise TypeError("EVAL_AGENT_INPUT: agent requires a nonempty prompt")
    fields = {
        "agent": "agent", "cwd": "cwd", "isolation": "isolation", "iso_apply": "isoApply",
        "output_schema": "outputSchema", "schema_mode": "schemaMode",
        "timeout_seconds": "timeoutSeconds", "scope": "scope",
    }
    arguments = {"agent": "task"}
    for name, value in options.items():
        if name == "keep_alive":
            if type(value) is not bool:
                raise TypeError("EVAL_AGENT_INPUT: keep_alive must be boolean")
            continue
        if name not in fields:
            raise TypeError(f"EVAL_AGENT_INPUT: unknown option {name}")
        arguments[fields[name]] = value
    if type(arguments["agent"]) is not str or not arguments["agent"]:
        raise TypeError("EVAL_AGENT_INPUT: agent must be a nonempty name")
    # No caller-controlled field can replace the operation or select foreground.
    arguments["task"] = prompt
    if options.get("keep_alive", False):
        if (("isolation" in arguments and arguments["isolation"] != "none")
                or any(field in arguments for field in ("isoApply", "outputSchema", "schemaMode"))):
            raise TypeError("EVAL_AGENT_INPUT: warm workers require isolation none and do not accept schema or iso_apply options")
        arguments["operation"] = "worker_start"
        started = _agent_worker_snapshot(await _agent_native_result("subagent", arguments), turn_id=1)
        if started["agent"] != arguments["agent"]:
            raise ToolBridgeError("EVAL_AGENT_PROTOCOL: native worker agent changed")
        return _AgentWorker(started)
    arguments["async"] = True
    started = await _agent_native_result("subagent", arguments)
    if (type(started) is not dict or started.get("schema") != "pi.subagent.background.v1"
            or type(started.get("jobId")) is not str or not started["jobId"]):
        raise ToolBridgeError("EVAL_AGENT_PROTOCOL: invalid background acknowledgment")
    return _AgentJob(started["jobId"], arguments["agent"])


_AGENT_CLOCK = time.monotonic


async def _agent_wait(handles, timeout=30, raise_errors=True):
    if (type(handles) not in (list, tuple) or len(handles) > 32
            or any(type(handle) not in (_AgentJob, _AgentWorker, _AgentTurn, _AgentPoolItem, _ModelJob) for handle in handles)
            or type(raise_errors) is not bool):
        raise TypeError("EVAL_AGENT_INPUT: wait requires up to 32 agent/model handles and boolean raise_errors")
    deadline = _AGENT_CLOCK() + _agent_wait_ms(timeout) / 1000
    # Freeze each worker's current turn before the first await so concurrent
    # sends cannot retarget a mixed wait already in progress.
    selected = [_AgentTurn(handle, handle.turn_id) if type(handle) is _AgentWorker else handle
                for handle in handles]
    results = []
    for handle in selected:
        remaining = max(0, int((deadline - _AGENT_CLOCK()) * 1000))
        results.append(await _agent_wait_one(handle, remaining, raise_errors))
    return results


def _pool_bounded_text(value, maximum):
    if type(value) is not str or not value.strip() or "\0" in value:
        return False
    try:
        return len(value.encode("utf-8")) <= maximum
    except UnicodeError:
        return False


def _pool_terminal(status):
    return status not in ("open", "draining")


def _pool_snapshot(snapshot, pool_id=None, item_id=None):
    if (type(snapshot) is not dict or snapshot.get("schema") != "pi.subagent.pool.v1"
            or type(snapshot.get("poolId")) is not str or not snapshot["poolId"]
            or (pool_id is not None and snapshot["poolId"] != pool_id)
            or type(snapshot.get("jobId")) is not str or not snapshot["jobId"]
            or type(snapshot.get("agent")) is not str or not snapshot["agent"]
            or snapshot.get("status") not in ("open", "draining", "completed", "failed", "cancelled")
            or type(snapshot.get("sealed")) is not bool
            or type(snapshot.get("concurrency")) is not int or not 1 <= snapshot["concurrency"] <= 8
            or type(snapshot.get("items")) is not list or len(snapshot["items"]) > 32
            or type(snapshot.get("admittedItems")) is not int
            or snapshot["admittedItems"] != len(snapshot["items"])
            or type(snapshot.get("remainingMs")) is not int or snapshot["remainingMs"] < 0
            or any(type(snapshot.get(key)) is not int or not 0 <= snapshot[key] <= snapshot["admittedItems"]
                   for key in ("queuedItems", "runningItems", "completedItems", "failedItems"))):
        raise ToolBridgeError("EVAL_POOL_PROTOCOL: invalid native pool snapshot")
    counts = {"queued": 0, "running": 0, "completed": 0, "failed": 0, "cancelled": 0}
    for index, item in enumerate(snapshot["items"], 1):
        if (type(item) is not dict or type(item.get("id")) is not int or item["id"] != index
                or type(item.get("status")) is not str or item["status"] not in counts
                or type(item.get("summary")) is not str
                or type(item.get("resultRef")) is not dict
                or item["resultRef"].get("poolId") != snapshot["poolId"]
                or type(item["resultRef"].get("itemId")) is not int
                or item["resultRef"]["itemId"] != item["id"]):
            raise ToolBridgeError("EVAL_POOL_PROTOCOL: invalid native pool item manifest")
        counts[item["status"]] += 1
    if (snapshot["queuedItems"] != counts["queued"] or snapshot["runningItems"] != counts["running"]
            or snapshot["completedItems"] != counts["completed"]
            or snapshot["failedItems"] != counts["failed"] + counts["cancelled"]
            or (snapshot["status"] == "open") == snapshot["sealed"]):
        raise ToolBridgeError("EVAL_POOL_PROTOCOL: inconsistent native pool state")
    if item_id is not None or "itemId" in snapshot or "item" in snapshot:
        item = snapshot.get("item")
        selected = snapshot.get("itemId")
        if (type(selected) is not int or not 1 <= selected <= len(snapshot["items"])
                or (item_id is not None and selected != item_id)
                or type(item) is not dict or type(item.get("id")) is not int or item["id"] != selected
                or item.get("status") != snapshot["items"][selected - 1]["status"] or "result" not in item):
            raise ToolBridgeError("EVAL_POOL_PROTOCOL: invalid selected pool item")
        result = item["result"]
        if ((result is None and _agent_turn_done(item)) or (result is not None
                and (type(result) is not dict or type(result.get("content")) is not list
                     or "details" not in result or type(result.get("isError", False)) is not bool))):
            raise ToolBridgeError("EVAL_POOL_PROTOCOL: invalid native pool item result")
    return snapshot


async def _pool_read_item(handle, operation, timeout_ms=None):
    arguments = {"operation": operation, "poolId": handle.pool_id, "itemId": handle.item_id}
    if timeout_ms is not None:
        arguments["timeoutMs"] = timeout_ms
    snapshot = _pool_snapshot(await _agent_native_result("subagent", arguments), handle.pool_id, handle.item_id)
    if snapshot["jobId"] != handle.job_id or snapshot["agent"] != handle.agent:
        raise ToolBridgeError("EVAL_POOL_PROTOCOL: native pool identity changed")
    return snapshot


async def _pool_wait_item(handle, timeout_ms, raise_errors):
    snapshot = await _pool_read_item(handle, "pool_wait", timeout_ms)
    if not _agent_turn_done(snapshot["item"]):
        raise TimeoutError("EVAL_AGENT_TIMEOUT: pool item is still pending; the native pool remains available")
    result = snapshot["item"]["result"]
    if raise_errors and (snapshot["item"]["status"] != "completed" or result.get("isError", False)):
        raise ToolBridgeError(_agent_result_text(result) or f"Pool item {snapshot['item']['status']}")
    return result


class _AgentPoolItem:
    """One immutable native pool item. Only the pool can cancel its carrier."""

    __slots__ = ("_pool_id", "_item_id", "_job_id", "_agent")

    def __init__(self, pool, item_id):
        self._pool_id = pool["poolId"]
        self._item_id = item_id
        self._job_id = pool["jobId"]
        self._agent = pool["agent"]

    @property
    def id(self):
        return self._item_id

    @property
    def item_id(self):
        return self._item_id

    @property
    def pool_id(self):
        return self._pool_id

    @property
    def job_id(self):
        return self._job_id

    @property
    def agent(self):
        return self._agent

    async def status(self):
        return await _pool_read_item(self, "pool_result")

    async def done(self):
        return _agent_turn_done((await self.status())["item"])

    async def output(self):
        return (await self.status())["item"]["result"]

    async def wait(self, timeout=30):
        return await _pool_wait_item(self, _agent_wait_ms(timeout), True)

    def __await__(self):
        return self.wait().__await__()


class _AgentPool:
    """One native pool carrier; finish drains, close cancels running and queued work."""

    __slots__ = ("_options", "_name", "_concurrency", "_pool_id", "_job_id", "_snapshot", "_items",
                 "_closed", "_last_error", "_unknown_admission", "_admission_pending",
                 "_finish_acknowledged", "_close_acknowledged", "_lock")

    def __init__(self, options, name, concurrency):
        self._options = options
        self._name = name
        self._concurrency = concurrency
        self._pool_id = None
        self._job_id = None
        self._snapshot = None
        self._items = []
        self._closed = False
        self._unknown_admission = False
        self._admission_pending = False
        self._finish_acknowledged = False
        self._close_acknowledged = False
        self._last_error = None
        self._lock = asyncio.Lock()

    @property
    def id(self):
        return self._pool_id

    @property
    def pool_id(self):
        return self._pool_id

    @property
    def job_id(self):
        return self._job_id

    @property
    def name(self):
        return self._name

    @property
    def concurrency(self):
        return self._concurrency

    @property
    def closed(self):
        return self._closed

    @property
    def unknown_admission(self):
        return self._unknown_admission

    @property
    def items(self):
        return tuple(self._items)

    def _adopt(self, snapshot):
        _pool_snapshot(snapshot, self._pool_id)
        if (snapshot["agent"] != self._options["agent"] or snapshot["concurrency"] != self._concurrency
                or (self._job_id is not None and snapshot["jobId"] != self._job_id)
                or len(snapshot["items"]) < len(self._items)):
            raise ToolBridgeError("EVAL_POOL_PROTOCOL: native pool identity or retained items changed")
        self._pool_id = snapshot["poolId"]
        self._job_id = snapshot["jobId"]
        self._items.extend(_AgentPoolItem(snapshot, index + 1)
                           for index in range(len(self._items), len(snapshot["items"])))
        self._snapshot = snapshot
        self._closed |= snapshot["sealed"] or _pool_terminal(snapshot["status"])
        self._unknown_admission = False
        return snapshot

    def _view(self):
        if self._snapshot is None:
            snapshot = {"schema": "pi.eval.workpool.pending.v1", "poolId": None, "jobId": None,
                        "agent": self._options["agent"], "status": "unavailable" if self._unknown_admission
                        else "cancelled" if self._closed else "unstarted", "sealed": self._closed,
                        "concurrency": self._concurrency, "admittedItems": 0, "items": []}
        else:
            snapshot = {**self._snapshot, "items": [{**item, "resultRef": dict(item["resultRef"])}
                                                   for item in self._snapshot["items"]]}
        return {**snapshot, "name": self._name, "closed": self._closed,
                "unknownAdmission": self._unknown_admission, "lastError": self._last_error}

    async def _command(self, operation, **extra):
        return self._adopt(await _agent_native_result("subagent", {
            "operation": operation, "poolId": self._pool_id, **extra,
        }))

    def _batch_arguments(self, tasks):
        arguments = ({**self._options, "operation": "pool_start", "concurrency": self._concurrency, "tasks": tasks}
                     if self._pool_id is None else {"operation": "pool_push", "poolId": self._pool_id, "tasks": tasks})
        # Match the JS bridge's 256 KiB payload cap before any native mutation;
        # Python's larger outer protocol frame limit is not a pool API promise.
        try:
            encoded = json.dumps({"name": "subagent", "args": arguments}, ensure_ascii=False,
                                 allow_nan=False, separators=(",", ":")).encode("utf-8")
        except (UnicodeError, ValueError):
            raise TypeError("EVAL_POOL_INPUT: pool arguments must be valid UTF-8 JSON") from None
        if len(encoded) > 256 * 1024:
            raise ValueError("EVAL_POOL_INPUT: encoded batch exceeds 256 KiB; split tasks across smaller push() calls")
        return arguments

    async def push(self, tasks):
        if type(tasks) not in (list, tuple) or len(tasks) > 32:
            raise TypeError("EVAL_POOL_INPUT: push requires a list or tuple of up to 32 task strings")
        batch = tuple(tasks)
        if any(not _pool_bounded_text(task, 65536) for task in batch):
            raise TypeError("EVAL_POOL_INPUT: every task must be nonempty UTF-8 text of at most 65536 bytes without NUL")
        owner = _bridge_owner()
        async with self._lock:
            if _bridge_owner() is not owner:
                raise ToolBridgeError("EVAL_POOL_INTERRUPTED: queued command belongs to a completed cell")
            if self._closed:
                raise ToolBridgeError("EVAL_POOL_CLOSED: pool admission is closed; inspect items or close existing workers")
            if len(self._items) + len(batch) > 32:
                raise ValueError("EVAL_POOL_CAPACITY: at most 32 items may be admitted per pool")
            if not batch:
                return []
            arguments = self._batch_arguments(list(batch))
            try:
                self._admission_pending = True
                previous = len(self._items)
                snapshot = self._adopt(await _agent_native_result("subagent", arguments))
                ids = snapshot.get("admittedItemIds")
                if (type(ids) is not list or len(ids) != len(batch)
                        or any(type(item_id) is not int or not previous < item_id <= len(self._items)
                               or (index > 0 and item_id != ids[index - 1] + 1)
                               for index, item_id in enumerate(ids))):
                    raise ToolBridgeError("EVAL_POOL_PROTOCOL: invalid atomic batch acknowledgment")
                self._last_error = None
                return [self._items[item_id - 1] for item_id in ids]
            except BaseException as cause:
                # A batch is atomic natively, but a lost acknowledgment may
                # hide its admission. Never replay; peek reconciles known ids.
                self._closed = True
                self._unknown_admission = True
                self._last_error = str(cause) or type(cause).__name__
                if not isinstance(cause, Exception):
                    raise
                error = ToolBridgeError(
                    "EVAL_POOL_ADMISSION: batch acknowledgment failed; admission closed without retry. "
                    f"Retained handles remain in pool.items; peek() can reconcile a known pool. {self._last_error}"
                )
                error.admitted = tuple(self._items)
                error.requested = len(batch)
                raise error from cause
            finally:
                self._admission_pending = False

    async def _peek_locked(self):
        if self._pool_id is not None:
            await self._command("pool_status")
        return self._view()

    async def peek(self):
        owner = _bridge_owner()
        async with self._lock:
            if _bridge_owner() is not owner:
                raise ToolBridgeError("EVAL_POOL_INTERRUPTED: queued command belongs to a completed cell")
            return await self._peek_locked()

    async def status(self):
        owner = _bridge_owner()
        async with self._lock:
            if _bridge_owner() is not owner:
                raise ToolBridgeError("EVAL_POOL_INTERRUPTED: queued command belongs to a completed cell")
            return await self._peek_locked()

    async def finish(self):
        owner = _bridge_owner()
        async with self._lock:
            if _bridge_owner() is not owner:
                raise ToolBridgeError("EVAL_POOL_INTERRUPTED: queued command belongs to a completed cell")
            if self._close_acknowledged or (self._pool_id is None and self._closed):
                raise ToolBridgeError("EVAL_POOL_CLOSED: a cancelled or unacknowledged pool cannot be finished")
            self._closed = True
            if not self._finish_acknowledged:
                try:
                    if self._pool_id is None:
                        arguments = self._batch_arguments([])
                        self._admission_pending = True
                        self._adopt(await _agent_native_result("subagent", arguments))
                        self._admission_pending = False
                    snapshot = await self._command("pool_finish")
                    if not snapshot["sealed"]:
                        raise ToolBridgeError("EVAL_POOL_PROTOCOL: pool finish did not seal admission")
                    self._finish_acknowledged = True
                except BaseException as error:
                    self._unknown_admission |= self._admission_pending
                    self._last_error = str(error) or type(error).__name__
                    raise
                finally:
                    self._admission_pending = False
            return _AgentJob(self._job_id, self._options["agent"])

    async def wait(self, timeout=30):
        deadline = _AGENT_CLOCK() + _agent_wait_ms(timeout) / 1000
        job = await self.finish()
        return await _agent_wait_one(job, max(0, int((deadline - _AGENT_CLOCK()) * 1000)), True)

    async def output(self):
        owner = _bridge_owner()
        async with self._lock:
            if _bridge_owner() is not owner:
                raise ToolBridgeError("EVAL_POOL_INTERRUPTED: queued command belongs to a completed cell")
            if self._job_id is None:
                return None
            return (await _agent_read_job(self._job_id, "wait", 0)).get("result")

    async def close(self):
        owner = _bridge_owner()
        async with self._lock:
            if _bridge_owner() is not owner:
                raise ToolBridgeError("EVAL_POOL_INTERRUPTED: queued command belongs to a completed cell")
            self._closed = True
            if self._pool_id is not None and not self._close_acknowledged:
                try:
                    snapshot = await self._command("pool_close")
                    if not _pool_terminal(snapshot["status"]):
                        raise ToolBridgeError("EVAL_POOL_PROTOCOL: pool close did not acknowledge termination")
                    self._close_acknowledged = True
                except BaseException as error:
                    self._last_error = str(error) or type(error).__name__
                    raise
            all_known_closed = self._pool_id is None or self._close_acknowledged
            return {**self._view(), "closed": True,
                    "allClosed": all_known_closed and not self._unknown_admission,
                    "allKnownClosed": all_known_closed, "unknownAdmission": self._unknown_admission}

    async def cancel(self):
        return await self.close()

    def __await__(self):
        return self.wait().__await__()


async def _workpool(**options):
    native = {"agent": "task"}
    name = "workpool"
    concurrency = 4
    for key, value in options.items():
        if key == "name":
            if not _pool_bounded_text(value, 128):
                raise TypeError("EVAL_POOL_INPUT: name must be nonempty text of at most 128 UTF-8 bytes without NUL")
            name = value
        elif key == "concurrency":
            if type(value) is not int or not 1 <= value <= 8:
                raise ValueError("EVAL_POOL_INPUT: concurrency must be an integer from 1 to 8")
            concurrency = value
        elif key == "agent":
            if not _pool_bounded_text(value, 128):
                raise TypeError("EVAL_POOL_INPUT: agent must be nonempty text of at most 128 UTF-8 bytes without NUL")
            native[key] = value
        elif key == "cwd":
            if type(value) is not str or not value or "\0" in value:
                raise TypeError("EVAL_POOL_INPUT: cwd must be a nonempty path without NUL")
            native[key] = value
        elif key == "scope":
            if value not in ("user", "project", "both"):
                raise TypeError("EVAL_POOL_INPUT: scope must be user, project or both")
            native[key] = value
        elif key == "isolation":
            if value != "none":
                raise TypeError("EVAL_POOL_INPUT: workpool requires isolation none")
            native[key] = value
        elif key == "timeout_seconds":
            if type(value) is not int or not 1 <= value <= 86400:
                raise ValueError("EVAL_POOL_INPUT: timeout_seconds must be an integer from 1 to 86400")
            native["timeoutSeconds"] = value
        else:
            raise TypeError(f"EVAL_POOL_INPUT: unknown option {key}")
    pool = _AgentPool(native, name, concurrency)
    pool._batch_arguments([])
    return pool


NAMESPACE["agent"] = _agent
NAMESPACE["wait"] = _agent_wait
NAMESPACE["workpool"] = _workpool
NAMESPACE["completion"] = _completion
NAMESPACE["judge"] = _judge
NAMESPACE["judge_batch"] = _judge_batch


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
    owner = _OUTPUT_CONTEXT.get()
    if owner is not None:
        # A soft stop may interrupt subprocess creation or asynchronous pump
        # cleanup. Never infer that all installer descendants settled just
        # because the root Python task returned; the host must retire its tree.
        owner.package_operation = True
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


class _SoftTimeout(BaseException):
    """A soft stop is not an ordinary application error to silently ignore."""


def _soft_alarm(_signum, _frame):
    budget = _SOFT_DEADLINE
    # This dispatcher stays installed after opt-in. An already pending alarm
    # must never terminate the idle kernel or interrupt the next cell early.
    if (budget is None or not budget.active or budget.output.soft_timed_out
            or time.monotonic() < budget.deadline):
        return
    budget.output.soft_timed_out = True
    exchanging = _BRIDGE_BUSY or _BRIDGE_READER is not None
    if exchanging:
        # Do not inject an exception between sending a request and owning its
        # reader, or bypass the async reader's cancellation/drain loop. The
        # host's hard deadline still bounds an unresponsive native exchange.
        budget.output.bridge_interrupted = True
    if budget.task is not None and not budget.task.done():
        # Wake a selector blocked in top-level await. Never leave the root
        # coroutine pending merely because a signal arrived between tasks.
        _CELL_LOOP.call_soon_threadsafe(budget.task.cancel)
    if _OUTPUT_CONTEXT.get() is budget.output and not exchanging:
        # Synchronous Python (including time.sleep) needs an exception; a
        # queued asyncio cancellation alone cannot stop a busy Python loop.
        # Do not inject it into an unrelated retained task's context.
        raise _SoftTimeout("EVAL_SOFT_TIMEOUT: cell soft deadline exceeded")


class _SoftDeadline:
    def __init__(self, seconds, output):
        self.seconds = seconds
        self.output = output
        self.task = None
        self.active = False
        self.deadline = 0.0

    def start(self):
        global _SOFT_DEADLINE
        if type(self.seconds) is not int or not 1 <= self.seconds <= 599:
            raise ValueError("EVAL_INPUT: softTimeoutSecs must be an integer from 1 through 599")
        if not all(hasattr(signal, name) for name in (
                "SIGALRM", "ITIMER_REAL", "getitimer", "setitimer")):
            raise RuntimeError("EVAL_SOFT_TIMEOUT_UNAVAILABLE: this interpreter has no POSIX interval timer")
        if (signal.getsignal(signal.SIGALRM) not in (signal.SIG_DFL, signal.SIG_IGN, _soft_alarm)
                or any(signal.getitimer(signal.ITIMER_REAL))):
            raise RuntimeError("EVAL_SOFT_TIMEOUT_BUSY: an existing SIGALRM handler or timer owns the signal")
        if (hasattr(signal, "pthread_sigmask")
                and signal.SIGALRM in signal.pthread_sigmask(signal.SIG_BLOCK, [])):
            raise RuntimeError("EVAL_SOFT_TIMEOUT_BLOCKED: SIGALRM is blocked on the kernel thread")
        if _SOFT_DEADLINE is not None:
            raise RuntimeError("EVAL_SOFT_TIMEOUT_BUSY: another cell owns the soft deadline")
        # Reserve only an unused signal. Do not cancel a caller's timer or
        # replace a custom handler. The kernel-owned dispatcher ignores late
        # delivery between cells and verifies the next cell's own deadline.
        signal.signal(signal.SIGALRM, _soft_alarm)
        self.deadline = time.monotonic() + self.seconds
        self.active = True
        _SOFT_DEADLINE = self
        try:
            signal.setitimer(signal.ITIMER_REAL, self.seconds)
        except BaseException:
            self.active = False
            _SOFT_DEADLINE = None
            raise

    def close(self):
        global _SOFT_DEADLINE
        if self.active:
            self.active = False
            _SOFT_DEADLINE = None
            signal.setitimer(signal.ITIMER_REAL, 0)


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
    completed = False
    try:
        try:
            if output.soft_timed_out:
                raise _SoftTimeout("EVAL_SOFT_TIMEOUT: cell soft deadline exceeded before execution")
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
        completed = True
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
        output.finalized = completed


def run_cell(code, cell_id, soft_timeout_secs=None):
    displays = _DisplaySink()
    output = _CellOutput(cell_id)
    stdout = _CellStream(output, "stdout")
    stderr = _CellStream(output, "stderr")
    old_out, old_err = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = stdout, stderr
    budget = _SoftDeadline(soft_timeout_secs, output) if soft_timeout_secs is not None else None
    task = None
    try:
        # Preflight the optional timer before creating a runnable coroutine.
        # A refused timer must not leave unstarted code queued for a later cell.
        if budget is not None:
            budget.start()
        execution = _execute_cell(code, displays, output)
        try:
            task = _CELL_CONTEXT.run(_CELL_LOOP.create_task, execution)
        except BaseException:
            execution.close()
            raise
        if budget is not None:
            budget.task = task
        ok, result, error = _CELL_CONTEXT.run(
            _CELL_LOOP.run_until_complete, task
        )
    except BaseException:  # noqa: BLE001 - full traceback back to the host
        ok, result, error = False, None, _error_text()
    finally:
        if budget is not None:
            try:
                if budget.active and time.monotonic() >= budget.deadline:
                    output.soft_timed_out = True
                budget.close()
            except BaseException:
                output.kernel_error = "EVAL_KERNEL_INCOMPLETE: soft timer cleanup failed"
        if task is not None and (not task.done() or not output.finalized):
            # A root task that escaped run_until_complete cannot be reused on
            # the next request. Native validation retires this process rather
            # than running old code under the next cell's authority.
            task.cancel()
            output.kernel_error = "EVAL_KERNEL_INCOMPLETE: root cell task or finalization did not settle"
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
    if output.soft_timed_out:
        ok, result = False, None
        error = (error or "") + (
            "\nEVAL_SOFT_TIMEOUT: cell interrupted; completed mutations are not rolled back. "
            "The host retains state only after validating complete output and a settled cell.\n"
        )
        if output.package_operation:
            output.kernel_error = "EVAL_KERNEL_INCOMPLETE: soft-timed-out package operation requires process-tree retirement"
        if output.bridge_interrupted:
            output.kernel_error = "EVAL_KERNEL_INCOMPLETE: soft deadline expired during a tool exchange; reply drained, process-tree retirement required"
    if output.kernel_error is not None:
        ok, result = False, None
        error = (error or "") + "\n" + output.kernel_error
    if _BRIDGE_FAILURE is not None:
        ok, result, error = False, None, (error or "") + "\n" + _BRIDGE_FAILURE
        output.bridge_error = output.bridge_error or _BRIDGE_FAILURE
    return ok, result, error, displays.frames, sequence, output


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
            ok, result, error, displays, sequence, output = run_cell(
                request.get("code", ""), request.get("id"), request.get("softTimeoutSecs")
            )
            response = {
                "id": request.get("id"),
                "ok": ok,
                "stdout": "",
                "stderr": "",
                "displays": displays,
                "outputSequence": sequence,
            }
            if output.bridge_error is not None:
                response["bridgeError"] = output.bridge_error
            if output.kernel_error is not None:
                response["kernelError"] = output.kernel_error
            if output.soft_timed_out:
                response["softTimeout"] = True
            if ok:
                response["result"] = result
            else:
                response["error"] = error
        except Exception as exc:  # noqa: BLE001 - protocol-level failure
            response = {"id": None, "ok": False, "stdout": "", "stderr": "", "error": str(exc)}
        _send_frame(response)
        if _BRIDGE_FAILURE is not None or response.get("kernelError") is not None:
            # Never reinterpret reply bytes or resume an unsettled root as
            # the next cell. Native ownership still retires the process tree.
            break


if __name__ == "__main__":
    main()
