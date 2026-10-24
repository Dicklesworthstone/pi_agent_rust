"""Execute the production Python facade and bootstrap with controlled transports.

These checks do not substitute for Rust, Chromium or live-site acceptance.
"""

import asyncio
import json
from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "src/eval/python_browser.py"
MAKE_BROWSER = runpy.run_path(str(SOURCE))["make_browser"]


def result(details=None, content=None, failed=False):
    return {"schema": "pi.eval.tool_result.v1", "content": content or [],
            "details": {} if details is None else details, "isError": failed}


class Host:
    def __init__(self):
        self.calls = []
        self.reply = lambda args: result(args)

    async def call(self, name, args):
        if name != "browser":
            raise AssertionError("unexpected native tool")
        self.calls.append(args)
        return self.reply(args)


class BrowserTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.host = Host()
        self.browser = MAKE_BROWSER(self.host.call)

    async def test_open_retains_complete_admission_and_literal_scope(self):
        native = result({"url": "https://example.test"}, [{"type": "text", "text": "loaded"}])
        self.host.reply = lambda args: native
        page = await self.browser.open(name="app", url="https://example.test", timeout_ms=5000)
        self.assertEqual(page.name, "app")
        self.assertIs(page.opened, native)
        self.assertEqual(self.host.calls, [{"timeout_ms": 5000, "url": "https://example.test",
                                          "action": "open", "tab": "app"}])

    async def test_semantics_and_literal_accessible_names(self):
        page = self.browser.tab("app")
        await page.get_by_label("Email").fill("person@example.test")
        await page.get_by_role("button", 'Save "draft"').click(wait_after={"selector": "#status", "text": "Saved"})
        await page.get_by_text("literal 雪").hover()
        await page.get_by_test_id("result").wait_for(state="attached")
        self.assertEqual([call["selector"] for call in self.host.calls], [
            "label/Email", 'role/button[name="Save \\"draft\\"" exact]', "text/literal 雪", "testid/result"])
        self.assertEqual(self.host.calls[1]["wait_after"], {"selector": "#status", "text": "Saved"})

    async def test_frame_scope_and_references_are_explicit_on_every_call(self):
        frame = self.browser.tab("app").frame("native-frame")
        await frame.snapshot()
        await frame.ref("@e3").press("Enter")
        await frame.call("scroll", selector=".list", delta_y=100)
        self.assertEqual([call["frame"] for call in self.host.calls], ["native-frame"] * 3)
        self.assertEqual([call["tab"] for call in self.host.calls], ["app"] * 3)
        self.assertEqual(self.host.calls[1]["selector"], "@e3")

    async def test_dynamic_decision_consumes_native_snapshot_before_input(self):
        self.host.reply = lambda args: result({"snapshot": {"elements": [
            {"text": "Save", "ref_id": "@e7"}, {"text": "Cancel", "ref_id": "@e8"}]}})
        page = self.browser.tab("app")
        seen = await page.snapshot()
        chosen = next(e for e in seen["details"]["snapshot"]["elements"] if e["text"] == "Save")
        await page.ref(chosen["ref_id"]).click()
        self.assertEqual(self.host.calls, [{"action": "snapshot", "tab": "app"},
                                          {"selector": "@e7", "action": "click", "tab": "app"}])

    async def test_batch_is_one_native_call_and_preserves_bindings_assertions(self):
        steps = [
            {"call": {"action": "snapshot"}, "expect": {"pointer": "/ok", "equals": False}},
            {"call": {"action": "drag", "target_selector": "testid/done"},
             "bind": {"selector": {"step": 0, "pointer": "/snapshot/elements/0/ref_id"}}},
        ]
        await self.browser.tab("board").batch(steps, timeout_ms=15000)
        self.assertEqual(len(self.host.calls), 1)
        wire = self.host.calls[0]
        self.assertEqual(wire["action"], "batch")
        self.assertEqual(wire["timeout_ms"], 15000)
        self.assertEqual(wire["steps"][1]["call"]["tab"], "board")
        self.assertEqual(wire["steps"][1]["bind"], steps[1]["bind"])
        self.assertIs(wire["steps"][0]["expect"]["equals"], False)
        self.assertNotIn("tab", steps[0]["call"])
        self.assertEqual(steps[0]["call"]["action"], "snapshot")

    async def test_malformed_later_batch_item_sends_no_prefix(self):
        first = {"call": {"action": "snapshot"}}
        for steps in ([first, {}], [first, {"call": {"action": "click", "tab": "other"}}],
                      [first, {"call": {"action": "reload"}, "unknown": True}], [], [first] * 17):
            with self.subTest(steps=steps), self.assertRaises(TypeError):
                await self.browser.tab("app").batch(steps)
        self.assertEqual(self.host.calls, [])

    async def test_native_action_error_carries_partial_result_and_never_replays(self):
        failed = result({"completedSteps": 1}, [{"type": "text", "text": "postcondition failed"}], True)
        self.host.reply = lambda args: failed
        with self.assertRaises(RuntimeError) as caught:
            await self.browser.tab("app").locator("#save").click()
            await self.browser.tab("app").reload()
        self.assertEqual(type(caught.exception).__name__, "BrowserActionError")
        self.assertIsInstance(caught.exception, self.browser.ActionError)
        self.assertEqual(caught.exception.action, "click")
        self.assertIs(caught.exception.result, failed)
        self.assertEqual(len(self.host.calls), 1)

    async def test_batch_error_keeps_native_completed_step_evidence(self):
        failed = result({"completedSteps": 1, "failedStep": 1}, failed=True)
        self.host.reply = lambda args: failed
        with self.assertRaises(RuntimeError) as caught:
            await self.browser.tab("app").batch([{"call": {"action": "snapshot"}}])
        self.assertEqual(caught.exception.action, "batch")
        self.assertEqual(caught.exception.result["details"], {"completedSteps": 1, "failedStep": 1})
        self.assertEqual(len(self.host.calls), 1)

    async def test_retained_locator_reenters_current_policy(self):
        target = self.browser.tab("app").locator("#save")
        await target.click()

        def denied(args):
            raise PermissionError("current host denied browser")

        self.host.reply = denied
        with self.assertRaisesRegex(PermissionError, "current host denied"):
            await target.click()
        self.assertEqual(len(self.host.calls), 2)

    async def test_captured_transport_does_not_follow_public_method_replacement(self):
        self.host.call = None
        await self.browser.tab("app").snapshot()
        self.assertEqual(len(self.host.calls), 1)

    async def test_scope_and_fixed_input_are_not_overrideable(self):
        page = self.browser.tab("app")
        operations = [
            self.browser.open(name="a", url="https://example.test", action="stop"),
            page.snapshot(tab="other"), page.frame("one").snapshot(frame="two"),
            page.locator("#x").fill("one", text="two"),
            page.locator("#x").click(selector="#y"), page.goto("https://example.test", tab="other"),
            page.batch([{"call": {"action": "snapshot"}}], frame="other"),
        ]
        for operation in operations:
            with self.assertRaises(TypeError):
                await operation
        self.assertEqual(self.host.calls, [])

    async def test_drag_requires_same_facade_tab_and_frame(self):
        page = self.browser.tab("app")
        source = page.locator("#source")
        await source.drag_to(page.locator("#target"), drag_steps=12)
        self.assertEqual(self.host.calls[0], {"action": "drag", "tab": "app", "selector": "#source",
                                            "target_selector": "#target", "drag_steps": 12})
        for target in [self.browser.tab("other").locator("#target"), page.frame("f").locator("#target"),
                       {"selector": "#target"}, MAKE_BROWSER(self.host.call).tab("app").locator("#target")]:
            with self.assertRaises(TypeError):
                await source.drag_to(target)
        self.assertEqual(len(self.host.calls), 1)

    async def test_literal_forms_paths_and_full_image_result(self):
        field = self.browser.tab("app").locator("#field")
        await field.fill("")
        await field.type("literal {previous}")
        await field.upload(["reports/雪.csv"])
        await field.download("downloads/result.csv")
        self.assertEqual([c["action"] for c in self.host.calls], ["fill", "type", "upload", "download"])
        self.assertEqual(self.host.calls[0]["text"], "")
        self.assertEqual(self.host.calls[1]["text"], "literal {previous}")
        image = result({"width": 3}, [{"type": "image", "data": "bytes", "mimeType": "image/png"}])
        self.host.reply = lambda args: image
        self.assertIs(await self.browser.tab("app").screenshot(), image)

    async def test_arguments_are_copied_before_waiting_on_native_result(self):
        received = asyncio.Event()
        release = asyncio.Event()
        calls = []

        async def transport(name, args):
            calls.append(args)
            received.set()
            await release.wait()
            return result()

        browser = MAKE_BROWSER(transport)
        after = {"selector": "#status", "text": "Saved"}
        task = asyncio.create_task(browser.tab("app").locator("#save").click(wait_after=after))
        await received.wait()
        after["text"] = "Changed"
        release.set()
        await task
        self.assertEqual(calls[0]["wait_after"]["text"], "Saved")

    async def test_json_types_do_not_coerce_or_run_user_hooks(self):
        class Hostile(dict):
            def items(self):
                self.fail("custom hook must not run")

        for value in [float("nan"), float("inf"), 1 << 80, -(1 << 64), (), set(), object(),
                      lambda: None, Hostile(x=1), {1: "not a string"}, "\ud800"]:
            with self.subTest(value_type=type(value)), self.assertRaises(TypeError):
                await self.browser.tab("app").call("snapshot", extra=value)
        self.assertEqual(self.host.calls, [])

    async def test_cycles_depth_nodes_and_encoded_budget_refuse_before_dispatch(self):
        cycle = {}
        cycle["self"] = cycle
        deep = {}
        for _ in range(34):
            deep = {"next": deep}
        for value in [cycle, deep, [1] * 16385, "x" * 262145, "\x01" * 50000]:
            with self.assertRaises(TypeError):
                await self.browser.tab("app").call("snapshot", extra=value)
        self.assertEqual(self.host.calls, [])

    async def test_typed_null_false_and_literal_keys_are_unchanged(self):
        await self.browser.tab("app").call("evaluate", arguments={"a/b~雪": [None, False, 0, ""]})
        self.assertEqual(self.host.calls[0]["arguments"], {"a/b~雪": [None, False, 0, ""]})

    async def test_malformed_native_results_fail_without_empty_success(self):
        malformed = [None, {}, {**result(), "isError": 0}, {**result(), "content": None},
                     {"schema": "pi.eval.tool_result.v1", "content": [], "isError": False}]
        for reply in malformed:
            self.host.reply = lambda args, reply=reply: reply
            with self.assertRaisesRegex(RuntimeError, "PROTOCOL"):
                await self.browser.tab("app").snapshot()
        self.assertEqual(len(self.host.calls), len(malformed))

    async def test_handles_are_immutable_and_lifecycle_calls_are_explicit(self):
        page = self.browser.tab("app")
        for handle, attr in [(self.browser, "tab"), (page, "name"), (page.locator("#x"), "selector")]:
            with self.assertRaises(AttributeError):
                setattr(handle, attr, "changed")
            with self.assertRaises(AttributeError):
                delattr(handle, attr)
        await self.browser.status()
        await self.browser.tabs()
        await self.browser.start()
        await self.browser.stop()
        self.assertEqual([c["action"] for c in self.host.calls], ["status", "list_tabs", "start", "stop"])

    async def test_evaluate_accepts_only_explicit_source_not_a_callable(self):
        with self.assertRaises(TypeError):
            await self.browser.tab("app").evaluate(lambda: 42)
        await self.browser.tab("app").evaluate("({title:document.title})")
        self.assertEqual(self.host.calls, [{"action": "evaluate", "tab": "app", "script": "({title:document.title})"}])

    async def test_navigation_and_accessibility_methods_preserve_native_actions(self):
        page = self.browser.tab("app")
        await page.goto("https://example.test")
        await page.back()
        await page.forward()
        await page.reload()
        await page.accessibility()
        await page.frames()
        await page.scroll(delta_y=100)
        await page.press("Ctrl+a")
        await page.close()
        self.assertEqual([c["action"] for c in self.host.calls],
                         ["goto", "back", "forward", "reload", "ax_tree", "list_frames", "scroll", "press", "close"])
        self.assertTrue(all(c["tab"] == "app" for c in self.host.calls))

    async def test_empty_and_wrong_typed_controls_fail_before_native_calls(self):
        for name in [None, "", "  ", 7, "x\0y", "雪" * 22000]:
            with self.assertRaises(TypeError):
                self.browser.tab(name)
        page = self.browser.tab("app")
        for role, name, exact in [("BUTTON", "Save", True), ("button", "", True), ("button", "Save", 1)]:
            with self.assertRaises(TypeError):
                page.get_by_role(role, name, exact)
        with self.assertRaises(TypeError):
            await page.locator("#field").fill(42)
        with self.assertRaises(TypeError):
            await page.locator("#file").upload([])
        self.assertEqual(self.host.calls, [])

    async def test_cancellation_drops_pending_wait_and_does_not_repeat_action(self):
        entered = asyncio.Event()
        retired = asyncio.Event()
        calls = []

        async def transport(name, args):
            calls.append(args)
            entered.set()
            try:
                await asyncio.Future()
            finally:
                retired.set()

        browser = MAKE_BROWSER(transport)
        task = asyncio.create_task(browser.tab("app").locator("#save").click())
        await entered.wait()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertTrue(retired.is_set())
        self.assertEqual(len(calls), 1)


class BootstrapTests(unittest.TestCase):
    def test_staged_bootstrap_installs_before_kernel_main_without_changing_workspace_imports(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            staged = root / "private"
            staged.mkdir()
            helper = staged / "browser.py"
            helper.write_bytes(SOURCE.read_bytes())
            kernel = staged / "kernel.py"
            kernel.write_text('''import asyncio, json, sys
class Tool:
    async def result(self, name, args):
        return {"schema":"pi.eval.tool_result.v1","content":[],"details":args,"isError":False}
NAMESPACE = {"__name__":"__main__","tool":Tool()}
def main():
    import workspace_module
    assert __name__ == "_pi_eval_kernel"
    assert NAMESPACE["__name__"] == "__main__"
    assert "make_browser" not in NAMESPACE
    assert sys.path[0] == ""
    assert len(sys.argv) == 2 and sys.argv[1].endswith("kernel.py")
    assert NAMESPACE["recovery_installed"] is True
    result = asyncio.run(NAMESPACE["browser"].tab("app").get_by_label("Email").fill("value"))
    print(json.dumps({"result":result,"workspace":workspace_module.value}))
if __name__ == "__main__":
    raise AssertionError("kernel loop ran before browser installation")
''', encoding="utf-8")
            (root / "workspace_module.py").write_text("value = 42\n", encoding="utf-8")
            (root / "kernel.py").write_text("raise RuntimeError('wrong kernel path')\n", encoding="utf-8")
            recovery = staged / "agent_recovery.py"
            recovery.write_text("def install(kernel):\n    kernel.NAMESPACE['recovery_installed'] = True\n", encoding="utf-8")
            (root / "agent_recovery.py").write_text("raise RuntimeError('wrong recovery path')\n", encoding="utf-8")
            command = [sys.executable, "-c",
                       "import sys\nwith open(sys.argv[1], encoding='utf-8') as source:\n code = source.read()\nexec(compile(code, sys.argv[1], 'exec'))",
                       str(helper), str(kernel), str(recovery)]
            run = subprocess.run(command, cwd=root, text=True, capture_output=True, timeout=10)
            self.assertEqual(run.returncode, 0, run.stderr)
            reply = json.loads(run.stdout)
            self.assertEqual(reply["workspace"], 42)
            self.assertEqual(reply["result"]["details"], {"action": "fill", "tab": "app", "selector": "label/Email", "text": "value"})

    def test_invalid_startup_does_not_run_a_kernel_loop(self):
        run = subprocess.run([sys.executable, str(SOURCE)], text=True, capture_output=True, timeout=10)
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("EVAL_BROWSER_BOOTSTRAP", run.stderr)
        self.assertEqual(run.stdout, "")


if __name__ == "__main__":
    unittest.main()
