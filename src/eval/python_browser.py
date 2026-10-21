"""Browser handles over the current cell's structured tool bridge.

This module owns no browser connection, native host or permission grant. Its
startup entry point installs the facade before the ordinary kernel loop starts.
"""

import json
import math
import re


def make_browser(native_result):
    """Capture only the guest transport; every operation uses current host policy."""
    kind = type
    dumps = json.dumps
    finite = math.isfinite
    role_pattern = re.compile(r"[a-z][a-z0-9-]*\Z")

    def invalid(message):
        return TypeError("EVAL_BROWSER_INPUT: " + message)

    def text(value, name):
        if kind(value) is not str or not value.strip() or "\0" in value:
            raise invalid(name + " must be nonempty literal text without NUL")
        try:
            size = len(value.encode("utf-8"))
        except UnicodeError:
            raise invalid(name + " must be valid UTF-8 text") from None
        if size > 65536:
            raise invalid(name + " exceeds 65536 UTF-8 bytes")
        return value

    def snapshot(value):
        remaining = 16384
        text_bytes = 0

        def charge(value):
            nonlocal text_bytes
            try:
                text_bytes += len(value.encode("utf-8"))
            except UnicodeError:
                raise invalid("arguments must contain valid UTF-8 text") from None
            if text_bytes > 262144:
                raise invalid("arguments exceed the text budget")

        def visit(item, depth):
            nonlocal remaining
            remaining -= 1
            if remaining < 0 or depth > 32:
                raise invalid("arguments exceed depth/node limits")
            item_type = kind(item)
            if item is None or item_type is bool:
                return item
            if item_type is int and -(1 << 63) <= item < (1 << 64):
                return item
            if item_type is float and finite(item):
                return item
            if item_type is str:
                charge(item)
                return item
            if item_type is list:
                return [visit(child, depth + 1) for child in item]
            if item_type is dict:
                copied = {}
                for key, child in item.items():
                    if kind(key) is not str:
                        raise invalid("argument object keys must be literal strings")
                    charge(key)
                    copied[key] = visit(child, depth + 1)
                return copied
            # Exact built-in types only: never call custom iterators, property
            # accessors or serializers, or coerce a tuple/object into JSON.
            raise invalid("arguments must contain only bounded JSON data")

        copied = visit(value, 0)
        if len(dumps(copied, ensure_ascii=False, allow_nan=False,
                     separators=(",", ":")).encode("utf-8")) > 262144:
            raise invalid("encoded arguments exceed 256 KiB")
        return copied

    def options(value, reserved):
        if kind(value) is not dict:
            raise invalid("options must be a plain dictionary")
        copied = snapshot(value)
        if any(key in copied for key in reserved):
            raise invalid("options cannot replace explicit action, tab, frame or input arguments")
        return copied

    class BrowserActionError(RuntimeError):
        def __init__(self, action, result):
            parts = []
            left = 4096
            for block in result["content"]:
                if kind(block) is dict and block.get("type") == "text" and kind(block.get("text")) is str:
                    part = block["text"][:left]
                    parts.append(part)
                    left -= len(part) + 1
                    if left <= 0:
                        break
            super().__init__("EVAL_BROWSER_ACTION: " + action + " failed. " + "\n".join(parts))
            self.action = action
            self.result = result

    async def invoke(args):
        result = await native_result("browser", snapshot(args))
        if (kind(result) is not dict or result.get("schema") != "pi.eval.tool_result.v1"
                or kind(result.get("content")) is not list or "details" not in result
                or kind(result.get("isError")) is not bool):
            raise RuntimeError("EVAL_BROWSER_PROTOCOL: invalid native browser result")
        if result["isError"]:
            raise BrowserActionError(args["action"], result)
        return result

    def scoped(name, frame_id, action, args, fixed=None):
        text(action, "action")
        fixed = {} if fixed is None else fixed
        copied = options(args, ("action", "tab", "frame", *fixed))
        copied.update(fixed)
        copied.update(action=action, tab=name)
        if frame_id is not None:
            copied["frame"] = frame_id
        return copied

    class Frozen:
        __slots__ = ()

        def __setattr__(self, name, value):
            raise AttributeError("browser handles are immutable")

        def __delattr__(self, name):
            raise AttributeError("browser handles are immutable")

    class Locator(Frozen):
        __slots__ = ("_name", "_frame_id", "selector")

        def __init__(self, name, frame_id, selector):
            object.__setattr__(self, "_name", name)
            object.__setattr__(self, "_frame_id", frame_id)
            object.__setattr__(self, "selector", text(selector, "selector"))

        def _call(self, action, args, **fixed):
            return invoke(scoped(self._name, self._frame_id, action, args,
                                 {"selector": self.selector, **fixed}))

        async def click(self, **args):
            return await self._call("click", args)

        async def hover(self, **args):
            return await self._call("hover", args)

        async def fill(self, value, **args):
            if kind(value) is not str:
                raise invalid("fill requires literal string text")
            return await self._call("fill", args, text=value)

        async def type(self, value, **args):
            if kind(value) is not str:
                raise invalid("type requires literal string text")
            return await self._call("type", args, text=value)

        async def press(self, key, **args):
            return await self._call("press", args, key=text(key, "key"))

        async def wait_for(self, **args):
            return await self._call("wait_for", args)

        async def upload(self, files, **args):
            if kind(files) is not list or not files or any(kind(path) is not str for path in files):
                raise invalid("upload requires a nonempty list of literal file paths")
            return await self._call("upload", args, files=files)

        async def download(self, path, **args):
            return await self._call("download", args, output_path=text(path, "output_path"))

        async def drag_to(self, target, **args):
            if (kind(target) is not Locator or target._name != self._name
                    or target._frame_id != self._frame_id):
                raise invalid("drag_to requires a locator in this same tab and frame")
            return await self._call("drag", args, target_selector=target.selector)

    class Tab(Frozen):
        __slots__ = ("name", "frame_id", "opened")

        def __init__(self, name, frame_id=None, opened=None):
            object.__setattr__(self, "name", text(name, "tab name"))
            object.__setattr__(self, "frame_id", None if frame_id is None else text(frame_id, "frame ID"))
            object.__setattr__(self, "opened", opened)

        async def call(self, action, **args):
            return await invoke(scoped(self.name, self.frame_id, action, args))

        def frame(self, frame_id):
            return Tab(self.name, text(frame_id, "frame ID"))

        def locator(self, selector):
            return Locator(self.name, self.frame_id, selector)

        def ref(self, selector):
            return self.locator(selector)

        def get_by_text(self, value):
            return self.locator("text/" + text(value, "text"))

        def get_by_label(self, value):
            return self.locator("label/" + text(value, "label"))

        def get_by_test_id(self, value):
            return self.locator("testid/" + text(value, "test ID"))

        def get_by_role(self, role, name, exact=True):
            if kind(role) is not str or role_pattern.fullmatch(role) is None or kind(exact) is not bool:
                raise invalid("get_by_role requires an accessible role and a boolean exact flag")
            return self.locator("role/" + role + "[name=" + dumps(text(name, "accessible name"),
                                ensure_ascii=False) + (" exact" if exact else "") + "]")

        async def goto(self, url, **args):
            return await invoke(scoped(self.name, self.frame_id, "goto", args, {"url": text(url, "URL")}))

        async def snapshot(self, **args):
            return await self.call("snapshot", **args)

        async def accessibility(self, **args):
            return await self.call("ax_tree", **args)

        async def frames(self, **args):
            return await self.call("list_frames", **args)

        async def screenshot(self, **args):
            return await self.call("screenshot", **args)

        async def scroll(self, **args):
            return await self.call("scroll", **args)

        async def press(self, key, **args):
            return await invoke(scoped(self.name, self.frame_id, "press", args, {"key": text(key, "key")}))

        async def back(self, **args):
            return await self.call("back", **args)

        async def forward(self, **args):
            return await self.call("forward", **args)

        async def reload(self, **args):
            return await self.call("reload", **args)

        async def close(self, **args):
            return await self.call("close", **args)

        async def evaluate(self, script, **args):
            return await invoke(scoped(self.name, self.frame_id, "evaluate", args,
                                       {"script": text(script, "script")}))

        async def batch(self, steps, **args):
            if kind(steps) is not list or not 1 <= len(steps) <= 16:
                raise invalid("batch requires 1-16 steps")
            prepared = []
            for step in snapshot(steps):
                if (kind(step) is not dict or kind(step.get("call")) is not dict
                        or any(key not in ("call", "expect", "bind") for key in step)):
                    raise invalid("each batch step requires call and optional bind/expect")
                fields = step["call"]
                action = fields.pop("action", None)
                step["call"] = scoped(self.name, self.frame_id, action, fields)
                prepared.append(step)
            copied = options(args, ("action", "tab", "frame", "steps"))
            if any(key != "timeout_ms" for key in copied):
                raise invalid("batch accepts only timeout_ms")
            return await invoke({**copied, "action": "batch", "steps": prepared})

    class Browser(Frozen):
        __slots__ = ()
        ActionError = BrowserActionError

        def tab(self, name):
            return Tab(name)

        async def open(self, *, name, url, **args):
            name = text(name, "tab name")
            opened = await invoke(scoped(name, None, "open", args, {"url": text(url, "URL")}))
            return Tab(name, opened=opened)

        async def tabs(self, **args):
            return await invoke({**options(args, ("action", "tab", "frame")), "action": "list_tabs"})

        async def status(self):
            return await invoke({"action": "status"})

        async def start(self):
            return await invoke({"action": "start"})

        async def stop(self):
            return await invoke({"action": "stop"})

    return Browser()


def _start_kernel():
    """Load the staged kernel once, install before main, preserve -c imports/argv."""
    import importlib.util
    import sys

    if len(sys.argv) != 3:
        raise RuntimeError("EVAL_BROWSER_BOOTSTRAP: expected the staged kernel path")
    kernel_path = sys.argv[2]
    # Restore the same argv shape as the previous direct kernel bootstrap.
    sys.argv[:] = [sys.argv[0], kernel_path]
    spec = importlib.util.spec_from_file_location("_pi_eval_kernel", kernel_path)
    if spec is None or spec.loader is None:
        raise RuntimeError("EVAL_BROWSER_BOOTSTRAP: kernel loader unavailable")
    kernel = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = kernel
    spec.loader.exec_module(kernel)
    kernel.NAMESPACE["browser"] = make_browser(kernel.NAMESPACE["tool"].result)
    kernel.main()


if __name__ == "__main__":
    _start_kernel()
