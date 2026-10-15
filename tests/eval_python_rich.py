"""Independent real-subprocess probes for the bundled eval Python server.

These do not run the Rust EvalTool, frontend, or authoritative DSR quality lane.
No package installation, network request, or copied implementation is used.
"""

import base64
import importlib.util
import io
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import unittest


ROOT = Path(__file__).resolve().parents[1]
SERVER = Path(os.environ.get("PI_TEST_KERNEL_PATH", ROOT / "src/eval/py_kernel_server.py"))


class Kernel:
    def __init__(self, *, no_site=False):
        self.directory = tempfile.TemporaryDirectory(prefix="pi-rich-display-test-")
        self.stderr = tempfile.TemporaryFile()
        self.process = subprocess.Popen(
            [sys.executable, *(["-S"] if no_site else []), "-u", str(SERVER)], cwd=self.directory.name,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr,
            text=True, encoding="utf-8",
            env={**os.environ, "MPLCONFIGDIR": self.directory.name,
                 "PYTHONDONTWRITEBYTECODE": "1"},
        )
        self.frames = queue.Queue()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        self.sequence = 0

    def _read(self):
        try:
            while True:
                line = self.process.stdout.readline(8 * 1024 * 1024)
                if not line:
                    break
                if not line.endswith("\n"):
                    raise AssertionError("Oversized or incomplete kernel frame")
                self.frames.put(json.loads(line))
        except BaseException as error:
            self.frames.put(error)
        finally:
            self.frames.put(EOFError("Kernel stdout closed"))

    def send(self, code):
        self.sequence += 1
        self.reply({"id": self.sequence, "code": code})
        return self.sequence

    def reply(self, frame):
        self.process.stdin.write(json.dumps(frame) + "\n")
        self.process.stdin.flush()

    def next(self):
        try:
            frame = self.frames.get(timeout=30)
        except queue.Empty as error:
            raise AssertionError("Kernel response exceeded the probe deadline") from error
        if isinstance(frame, BaseException):
            raise frame
        return frame

    def cell(self, code):
        identifier = self.send(code)
        displays = []
        console = []
        sequence = 0
        while True:
            frame = self.next()
            assert frame.get("id") == identifier, frame
            if "ok" in frame:
                assert frame["displays"] == displays, "Final result disagrees with streamed snapshots"
                assert frame["outputSequence"] == sequence, "Console sequence changed by a rich output"
                return frame, "".join(console)
            stream = frame["stream"]
            if stream["name"] == "display":
                assert stream["sequence"] == len(displays) + 1
                displays.append(stream["display"])
            else:
                sequence += 1
                assert stream["sequence"] == sequence
                assert stream["name"] in ("stdout", "stderr")
                console.append(stream["text"])

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        self.reader.join(timeout=3)
        self.process.stdout.close()
        self.stderr.close()
        self.directory.cleanup()


class KernelTestCase(unittest.TestCase):
    def setUp(self):
        self.kernel = Kernel()
        self.addCleanup(self.kernel.close)

    def good(self, code):
        result, console = self.kernel.cell(code)
        self.assertTrue(result["ok"], result.get("error"))
        return result, console

    def bad(self, code, diagnostic):
        result, console = self.kernel.cell(code)
        self.assertFalse(result["ok"], result)
        self.assertIn(diagnostic, result["error"])
        return result, console


class RichProtocolTests(KernelTestCase):
    def test_mime_bundle_streams_before_bridge_and_detaches_json(self):
        identifier = self.kernel.send("""
class Report:
    def _repr_mimebundle_(self, include, exclude):
        assert 'application/json' in include and 'text/html' not in include
        return {'application/json': payload, 'text/plain': 'fallback'}, {'application/json': {}}
payload = {'value': [42, True, None]}
display(Report())
payload['value'][0] = 99
assert tool.read('gate.txt') == 'release'
""")
        frame = self.kernel.next()
        self.assertEqual(frame["id"], identifier)
        self.assertEqual(frame["stream"]["name"], "display")
        self.assertEqual(frame["stream"]["display"]["data"], {"value": [42, True, None]})
        bridge = self.kernel.next()["bridge"]
        self.assertEqual(bridge["tool"], "read")
        self.kernel.reply({"bridge_result": {"call": bridge["call"], "ok": True, "content": "release"}})
        final = self.kernel.next()
        self.assertTrue(final["ok"], final.get("error"))
        self.assertEqual(final["displays"], [frame["stream"]["display"]])
        self.assertEqual(final["outputSequence"], 0)
        self.assertEqual(self.good("payload['value'][0]")[0]["result"], "99")

    def test_rich_last_expression_and_state_survive_multiple_cells(self):
        result, _ = self.good("""
class Report:
    def _repr_markdown_(self):
        return '## Result: 42'
report = Report()
report
""")
        self.assertIsNone(result["result"])
        self.assertEqual(result["displays"], [{"mimeType": "text/markdown", "data": "## Result: 42"}])
        self.assertEqual(self.good("report")[0]["displays"], result["displays"])
        self.assertEqual(self.good("{'answer': 42}")[0]["result"], "{'answer': 42}")
        self.assertEqual(self.good("2**80")[0]["result"], str(2**80))

    def test_explicit_mime_never_invokes_a_hook(self):
        result, _ = self.good("""
hooks = []
class Report:
    def _repr_json_(self):
        hooks.append('called')
        return {'answer': 42}
try:
    display(Report(), mime='application/json')
except TypeError as error:
    assert 'EVAL_DISPLAY_JSON' in str(error)
else:
    raise AssertionError('explicit JSON must reject objects')
assert hooks == []
display({'answer': 42}, mime='application/json')
""")
        self.assertEqual(result["displays"][0]["data"], {"answer": 42})

    def test_nested_objects_do_not_gain_implicit_serializers(self):
        self.bad("""
class Report:
    def _repr_json_(self):
        raise AssertionError('NESTED_HOOK_EXECUTED')
display({'nested': Report()})
""", "EVAL_DISPLAY_JSON")

    def test_malformed_preferred_bundle_is_not_silently_downgraded(self):
        result, _ = self.bad("""
class Report:
    def _repr_mimebundle_(self, **kwargs):
        return {'image/png': 'not-base64', 'text/plain': 'looks successful'}
display('accepted prefix')
display(Report())
""", "Error")
        self.assertEqual(result["displays"], [{"mimeType": "text/plain", "data": "accepted prefix"}])

    def test_bundle_json_null_and_metadata_are_unambiguous(self):
        result, _ = self.good("""
class Report:
    def _repr_mimebundle_(self, **kwargs):
        return {'application/json': None, 'text/plain': 'fallback'}, {'instructions': 'not output'}
display(Report())
""")
        self.assertEqual(result["displays"], [{"mimeType": "application/json", "data": None}])

    def test_hook_errors_do_not_hide_completed_effects_or_reset_namespace(self):
        result, _ = self.bad("""
keep = 42
class Report:
    def _repr_json_(self):
        raise RuntimeError('formatter failed')
display('prefix')
display(Report())
""", "formatter failed")
        self.assertEqual(len(result["displays"]), 1)
        self.assertEqual(self.good("keep")[0]["result"], "42")

    def test_html_hooks_are_never_called(self):
        result, _ = self.good("""
class Report:
    def _repr_html_(self):
        raise AssertionError('HTML_EXECUTED')
    def __repr__(self):
        return 'plain report'
Report()
""")
        self.assertEqual(result["result"], "plain report")
        self.assertEqual(result["displays"], [])

    def test_none_hook_declines_and_next_supported_hook_runs(self):
        result, _ = self.good("""
class Report:
    def _repr_png_(self):
        return None
    def _repr_markdown_(self):
        return '# available', {'isolated': True}
display(Report())
""")
        self.assertEqual(result["displays"], [{"mimeType": "text/markdown", "data": "# available"}])

    def test_formatter_cannot_reenter_display_and_recovers_after_catch(self):
        result, _ = self.good("""
class Report:
    def _repr_json_(self):
        display('recursive')
try:
    display(Report())
except RuntimeError as error:
    assert 'EVAL_DISPLAY_REENTRANT' in str(error)
else:
    raise AssertionError('recursive display accepted')
display({'recovered': True})
""")
        self.assertEqual(result["displays"], [{"mimeType": "application/json", "data": {"recovered": True}}])

    def test_count_limit_refuses_before_guest_formatter_side_effects(self):
        result, _ = self.good("""
class Report:
    def _repr_json_(self):
        raise AssertionError('FORMATTER_RAN_AT_CAPACITY')
for index in range(32):
    display(index)
try:
    display(Report())
except ValueError as error:
    assert 'EVAL_DISPLAY_LIMIT' in str(error)
else:
    raise AssertionError('count limit ignored')
42
""")
        self.assertEqual(len(result["displays"]), 32)
        self.assertEqual(result["result"], "42")

    def test_item_limit_and_nonfinite_json_are_still_enforced(self):
        self.bad("""
class Report:
    def _repr_markdown_(self):
        return '界' * 800000
display(Report())
""", "EVAL_DISPLAY_LIMIT")
        self.bad("""
class Report:
    def _repr_json_(self):
        return {'invalid': float('nan')}
display(Report())
""", "EVAL_DISPLAY_JSON")
        self.assertEqual(self.good("1 + 1")[0]["result"], "2")

    def test_copied_thread_context_refuses_before_formatter(self):
        result, _ = self.good("""
import asyncio
called = []
class Report:
    def _repr_json_(self):
        called.append(True)
        return {}
try:
    await asyncio.to_thread(display, Report())
except RuntimeError as error:
    assert 'EVAL_DISPLAY_CLOSED' in str(error)
else:
    raise AssertionError('thread display accepted')
assert called == []
""")
        self.assertEqual(result["displays"], [])

    def test_retained_old_task_cannot_run_formatter_in_later_cell(self):
        self.good("""
import asyncio
ready = asyncio.Event()
called = []
class Report:
    def _repr_json_(self):
        called.append(True)
        return {}
async def old():
    await ready.wait()
    try:
        display(Report())
    except RuntimeError as error:
        return str(error)
    return 'unexpected success'
pending = asyncio.create_task(old())
await asyncio.sleep(0)
""")
        result, _ = self.good("""
ready.set()
assert 'EVAL_DISPLAY_CLOSED' in await pending
assert called == []
display({'current': True})
""")
        self.assertEqual(result["displays"], [{"mimeType": "application/json", "data": {"current": True}}])

    def test_async_rich_result_uses_one_retained_event_loop(self):
        result, _ = self.good("""
import asyncio
class Report:
    def _repr_json_(self):
        return {'async': True}
await asyncio.sleep(0, result=Report())
""")
        self.assertIsNone(result["result"])
        self.assertEqual(result["displays"], [{"mimeType": "application/json", "data": {"async": True}}])


class ScientificDisplayTests(KernelTestCase):
    def need(self, *packages):
        missing = [name for name in packages if importlib.util.find_spec(name) is None]
        if missing:
            self.skipTest("Optional scientific dependencies unavailable: " + ", ".join(missing))

    def png(self, frame, size):
        from PIL import Image
        self.assertEqual(frame["mimeType"], "image/png")
        raw = base64.b64decode(frame["data"], validate=True)
        with Image.open(io.BytesIO(raw)) as image:
            image.load()  # Decode an actual raster, not just its PNG envelope.
            self.assertEqual(image.size, size)
            self.assertEqual(image.format, "PNG")
            return image.convert("RGBA").getpixel((0, 0))

    def test_plain_cells_do_not_import_optional_libraries(self):
        # Isolate the kernel from sitecustomize/.pth preloaders. This probe
        # environment already imports pandas/PIL before either kernel starts.
        kernel = Kernel(no_site=True)
        self.addCleanup(kernel.close)
        result, _ = kernel.cell("""
import sys
class Ordinary:
    pass
ordinary = Ordinary()
assert not any(name in sys.modules for name in ('matplotlib', 'pandas', 'PIL'))
ordinary
""")
        self.assertTrue(result["ok"], result.get("error"))
        self.assertEqual(result["displays"], [])
        after, _ = kernel.cell("assert not any(name in sys.modules for name in ('matplotlib', 'pandas', 'PIL'))")
        self.assertTrue(after["ok"], after.get("error"))

    def test_pillow_objects_and_trailing_results_are_detached_pngs(self):
        self.need("PIL")
        first, _ = self.good("""
from PIL import Image
image = Image.new('RGBA', (3, 2), (12, 34, 56, 78))
display(image)
image.putpixel((0, 0), (99, 88, 77, 66))
""")
        self.assertEqual(self.png(first["displays"][0], (3, 2)), (12, 34, 56, 78))
        later, _ = self.good("image")
        self.assertIsNone(later["result"])
        self.assertEqual(self.png(later["displays"][0], (3, 2)), (99, 88, 77, 66))

    def test_pillow_dimension_refusal_precedes_encoding(self):
        self.need("PIL")
        self.bad("""
from PIL import Image
image = Image.new('L', (16385, 1))
def forbidden_save(*args, **kwargs):
    raise AssertionError('ENCODER_RAN_BEFORE_DIMENSION_REFUSAL')
image.save = forbidden_save
display(image)
""", "EVAL_DISPLAY_PNG: dimensions exceed")
        self.assertEqual(self.good("image.size")[0]["result"], "(16385, 1)")

    def test_png_encoder_obeys_aggregate_budget_without_losing_prior_frames(self):
        self.need("PIL")
        result, _ = self.bad("""
import os
from PIL import Image
image = Image.frombytes('RGB', (1024, 1024), os.urandom(1024 * 1024 * 3))
display('a' * 1500000)
display('b' * 1500000)
display(image)
""", "EVAL_DISPLAY_LIMIT")
        self.assertEqual(len(result["displays"]), 2)
        self.assertTrue(all(frame["mimeType"] == "text/plain" for frame in result["displays"]))
        self.assertEqual(self.good("image.size")[0]["result"], "(1024, 1024)")

    def test_explicit_png_still_requires_bytes_not_an_image_object(self):
        self.need("PIL")
        self.bad("""
from PIL import Image
display(Image.new('RGB', (2, 2)), mime='image/png')
""", "EVAL_DISPLAY_PNG: expected PNG bytes")

    def test_matplotlib_figure_uses_native_size_and_retains_original_canvas(self):
        self.need("matplotlib", "PIL")
        result, _ = self.good("""
import matplotlib
import matplotlib.pyplot as plt
matplotlib.rcParams['savefig.dpi'] = 100000
matplotlib.rcParams['savefig.bbox'] = 'tight'
figure, axes = plt.subplots(figsize=(2, 1), dpi=100)
axes.plot([0, 1, 2], [0, 1, 0])
canvas = figure.canvas
display(figure)
assert figure.canvas is canvas
assert figure.dpi == 100
assert plt.fignum_exists(figure.number)
""")
        self.assertEqual(len(result["displays"]), 1)
        self.png(result["displays"][0], (200, 100))
        later, _ = self.good("figure")
        self.png(later["displays"][0], (200, 100))
        self.assertEqual(self.good("len(figure.axes)")[0]["result"], "1")

    def test_matplotlib_figure_without_pyplot_can_be_displayed(self):
        self.need("matplotlib", "PIL")
        result, _ = self.good("""
from matplotlib.figure import Figure
figure = Figure(figsize=(2, 1), dpi=50)
axes = figure.subplots()
axes.plot([1, 2], [2, 1])
figure
""")
        self.assertIsNone(result["result"])
        self.png(result["displays"][0], (100, 50))

    def test_matplotlib_oversized_canvas_is_refused_before_rendering(self):
        self.need("matplotlib")
        self.bad("""
from matplotlib.figure import Figure
figure = Figure(figsize=(200, 1), dpi=100)
def forbidden_save(*args, **kwargs):
    raise AssertionError('RASTERIZER_RAN_BEFORE_DIMENSION_REFUSAL')
figure.savefig = forbidden_save
display(figure)
""", "EVAL_DISPLAY_PNG: dimensions exceed")

    def test_pandas_dataframe_is_a_bounded_text_preview_not_coerced_json(self):
        self.need("pandas")
        result, _ = self.good("""
import pandas as pd
table = pd.DataFrame({'name': ['東京', 'Paris'], 'value': [42, float('nan')]})
display(table)
""")
        frame = result["displays"][0]
        self.assertEqual(frame["mimeType"], "text/plain")
        self.assertIn("東京", frame["data"])
        self.assertIn("NaN", frame["data"])
        self.assertIn("2 rows x 2 columns", frame["data"])
        self.assertIn("Pandas preview", frame["data"])
        later, _ = self.good("table")
        self.assertEqual(later["displays"], result["displays"])

    def test_large_pandas_preview_discloses_dimensions_and_truncation(self):
        self.need("pandas")
        result, _ = self.good("""
import pandas as pd
table = pd.DataFrame({f'col{index}': ['X' * 400] * 1000 for index in range(20)})
table
""")
        rendered = result["displays"][0]["data"]
        self.assertIn("1000 rows x 20 columns", rendered)
        self.assertIn("up to 40 rows, 12 columns", rendered)
        self.assertIn("...", rendered)
        self.assertNotIn("X" * 100, rendered)
        self.assertLess(len(rendered), 50000)
        self.assertEqual(self.good("table.shape")[0]["result"], "(1000, 20)")

    def test_pandas_series_preserves_labels_without_mutation(self):
        self.need("pandas")
        result, _ = self.good("""
import pandas as pd
series = pd.Series([1, pd.NA], index=['first', 'second'], dtype='Int64', name='reading')
display(series)
assert str(series.dtype) == 'Int64' and series.name == 'reading'
""")
        rendered = result["displays"][0]["data"]
        for expected in ("first", "second", "reading", "<NA>", "2 rows x 1 columns"):
            self.assertIn(expected, rendered)

    def test_loaded_script_rich_result_uses_retained_namespace(self):
        self.need("PIL")
        self.good("""
from pathlib import Path
Path('analysis.py').write_text("from PIL import Image\\npicture = Image.new('RGB', (4, 3))\\npicture\\n")
""")
        result, _ = self.good("%load analysis.py")
        self.png(result["displays"][0], (4, 3))
        self.assertEqual(self.good("picture.size")[0]["result"], "(4, 3)")

    def test_scientific_display_survives_later_error_without_becoming_success(self):
        self.need("PIL")
        result, _ = self.bad("""
from PIL import Image
picture = Image.new('RGB', (2, 3))
display(picture)
raise RuntimeError('after the image')
""", "after the image")
        self.png(result["displays"][0], (2, 3))
        self.png(self.good("picture")[0]["displays"][0], (2, 3))

    def test_pyplot_without_explicit_display_emits_and_closes_owned_figures(self):
        self.need("matplotlib", "PIL")
        result, _ = self.good("""
import matplotlib.pyplot as plt
figure, axes = plt.subplots(figsize=(2, 1), dpi=40)
axes.plot([0, 1, 2], [0, 1, 0])
axes.set_title('automatic')
""")
        self.assertEqual(len(result["displays"]), 1)
        self.png(result["displays"][0], (80, 40))
        later, _ = self.good("assert plt.get_fignums() == []; len(figure.axes)")
        self.assertEqual(later["result"], "1")
        self.assertEqual(later["displays"], [])

    def test_pyplot_show_and_reused_numbers_keep_creation_order(self):
        self.need("matplotlib", "PIL")
        for iteration in range(3):
            result, _ = self.good("""
import matplotlib.pyplot as plt
first = plt.figure(1, figsize=(2, 1), dpi=40)
second = plt.figure(2, figsize=(3, 1), dpi=40)
first.subplots().plot([0, 1], [1, 0])
second.subplots().plot([0, 1], [0, 1])
plt.show()
""")
            self.assertEqual(len(result["displays"]), 2, iteration)
            self.png(result["displays"][0], (80, 40))
            self.png(result["displays"][1], (120, 40))
            self.assertEqual(self.good("plt.get_fignums()")[0]["result"], "[]")

    def test_explicit_and_final_figures_are_closed_without_auto_duplicates(self):
        self.need("matplotlib", "PIL")
        result, _ = self.good("""
import matplotlib.pyplot as plt
first = plt.figure(figsize=(2, 1), dpi=40)
second = plt.figure(figsize=(3, 1), dpi=40)
display(first)
second
""")
        self.assertIsNone(result["result"])
        self.assertEqual(len(result["displays"]), 2)
        self.png(result["displays"][0], (80, 40))
        self.png(result["displays"][1], (120, 40))
        self.assertEqual(self.good("plt.get_fignums()")[0]["result"], "[]")
        # The Python Figure survives registry cleanup and can be edited later.
        later, _ = self.good("first.subplots().plot([1, 2]); display(first)")
        self.assertEqual(len(later["displays"]), 1)
        self.png(later["displays"][0], (80, 40))

    def test_closed_and_replaced_figure_number_is_not_captured_twice(self):
        self.need("matplotlib", "PIL")
        result, _ = self.good("""
import matplotlib.pyplot as plt
old = plt.figure(7, figsize=(2, 1), dpi=40)
plt.close(old)
replacement = plt.figure(7, figsize=(3, 1), dpi=40)
""")
        self.assertEqual(len(result["displays"]), 1)
        self.png(result["displays"][0], (120, 40))
        self.assertEqual(self.good("plt.get_fignums()")[0]["result"], "[]")

    def test_failed_body_still_emits_plot_without_masking_original_error(self):
        self.need("matplotlib", "PIL")
        result, _ = self.bad("""
import matplotlib.pyplot as plt
kept = 42
figure = plt.figure(figsize=(2, 1), dpi=40)
figure.subplots().plot([0, 1], [0, 1])
raise RuntimeError('ORIGINAL_CELL_FAILURE')
""", "ORIGINAL_CELL_FAILURE")
        self.png(result["displays"][0], (80, 40))
        later, _ = self.good("assert plt.get_fignums() == []; kept")
        self.assertEqual(later["result"], "42")
        self.assertEqual(later["displays"], [])

    def test_plot_failure_closes_owned_figures_and_preserves_healthy_siblings(self):
        self.need("matplotlib", "PIL")
        result, _ = self.bad("""
import matplotlib.pyplot as plt
oversized = plt.figure(figsize=(200, 1), dpi=100)
healthy = plt.figure(figsize=(2, 1), dpi=40)
display({'accepted': True})
raise RuntimeError('BODY_FAILED_TOO')
""", "BODY_FAILED_TOO")
        self.assertIn("EVAL_PLOT_OUTPUT", result["error"])
        self.assertIn("dimensions exceed", result["error"])
        self.assertEqual(result["displays"][0]["data"], {"accepted": True})
        self.assertEqual(len(result["displays"]), 2)
        self.png(result["displays"][1], (80, 40))
        later, _ = self.good("""
assert plt.get_fignums() == []
oversized.set_size_inches(1, 1)
display(oversized)
""")
        self.png(later["displays"][0], (100, 100))

    def test_stale_task_cannot_register_plot_in_later_cell(self):
        self.need("matplotlib", "PIL")
        self.good("""
import asyncio
import matplotlib.pyplot as plt
ready = asyncio.Event()
async def old():
    await ready.wait()
    try:
        plt.figure(99)
    except RuntimeError as error:
        return str(error)
    return 'unexpected success'
pending = asyncio.create_task(old())
await asyncio.sleep(0)
""")
        result, _ = self.good("""
ready.set()
assert 'EVAL_DISPLAY_CLOSED' in await pending
assert plt.get_fignums() == []
figure = plt.figure(figsize=(2, 1), dpi=40)
""")
        self.assertEqual(len(result["displays"]), 1)
        self.png(result["displays"][0], (80, 40))

    def test_copied_thread_cannot_register_a_current_cell_plot(self):
        self.need("matplotlib")
        result, _ = self.good("""
import asyncio
import matplotlib.pyplot as plt
try:
    await asyncio.to_thread(plt.figure)
except RuntimeError as error:
    assert 'EVAL_DISPLAY_CLOSED' in str(error)
else:
    raise AssertionError('cross-thread plot creation was accepted')
assert plt.get_fignums() == []
""")
        self.assertEqual(result["displays"], [])

    def test_unowned_process_global_plot_is_neither_published_nor_closed(self):
        self.need("matplotlib", "PIL")
        result, _ = self.good("""
import contextvars
import matplotlib.pyplot as plt
foreign = contextvars.Context().run(plt.figure, 99, figsize=(3, 1), dpi=40)
owned = plt.figure(1, figsize=(2, 1), dpi=40)
""")
        self.assertEqual(len(result["displays"]), 1)
        self.png(result["displays"][0], (80, 40))
        later, _ = self.good("assert plt.get_fignums() == [99]")
        self.assertEqual(later["displays"], [])
        adopted, _ = self.good("display(foreign); plt.close(foreign)")
        self.png(adopted["displays"][0], (120, 40))

    def test_dynamic_import_and_close_loops_do_not_exhaust_tracking(self):
        self.need("matplotlib", "PIL")
        result, _ = self.good("""
import importlib
plt = importlib.import_module('matplotlib.pyplot')
for index in range(40):
    temporary = plt.figure(figsize=(1, 1), dpi=20)
    plt.close(temporary)
figure = plt.figure(figsize=(2, 1), dpi=40)
""")
        self.assertEqual(len(result["displays"]), 1)
        self.png(result["displays"][0], (80, 40))

    def test_automatic_plots_use_shared_display_count_and_cleanup_on_refusal(self):
        self.need("matplotlib")
        result, _ = self.bad("""
import matplotlib.pyplot as plt
figure = plt.figure(figsize=(1, 1), dpi=20)
for index in range(32):
    display(index)
""", "EVAL_DISPLAY_LIMIT")
        self.assertEqual(len(result["displays"]), 32)
        self.assertEqual(self.good("plt.get_fignums()")[0]["result"], "[]")

    def test_non_agg_backend_is_not_automatically_captured_or_closed(self):
        self.need("matplotlib", "PIL")
        result, _ = self.good("""
import matplotlib
matplotlib.use('svg')
import matplotlib.pyplot as plt
figure = plt.figure(figsize=(2, 1), dpi=40)
""")
        self.assertEqual(result["displays"], [])
        later, _ = self.good("""
assert matplotlib.get_backend().casefold() == 'svg'
assert plt.fignum_exists(figure.number)
display(figure)
assert matplotlib.get_backend().casefold() == 'svg'
plt.close(figure)
""")
        self.png(later["displays"][0], (80, 40))

    def test_plot_draw_callback_cannot_create_unbounded_finalization_figures(self):
        self.need("matplotlib")
        result, _ = self.bad("""
import matplotlib.pyplot as plt
figure = plt.figure(figsize=(2, 1), dpi=40)
def recurse(event):
    plt.figure()
connection = figure.canvas.mpl_connect('draw_event', recurse)
""", "EVAL_DISPLAY_REENTRANT")
        self.assertEqual(result["displays"], [])
        later, _ = self.good("""
assert plt.get_fignums() == []
figure.canvas.mpl_disconnect(connection)
display(figure)
""")
        self.assertEqual(len(later["displays"]), 1)

    def test_open_figure_limit_fails_before_registering_an_extra_manager(self):
        self.need("matplotlib")
        result, _ = self.bad("""
import matplotlib.pyplot as plt
plt.rcParams['figure.max_open_warning'] = 0
for index in range(33):
    plt.figure(index + 1, figsize=(1, 1), dpi=20)
""", "at most 32 open cell-owned figures")
        self.assertEqual(len(result["displays"]), 32)
        later, _ = self.good("assert plt.get_fignums() == []")
        self.assertEqual(later["displays"], [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
