"""Real Python soft-deadline substrate checks, not native Rust acceptance."""

import os
import signal
import subprocess
import sys
import time
import unittest

from eval_python_bridge import Kernel


@unittest.skipUnless(hasattr(signal, "setitimer"), "POSIX interval timers required")
class SoftTimeoutTests(unittest.TestCase):
    def setUp(self):
        self.kernel = Kernel()
        self.addCleanup(self.kernel.close)

    def good(self, code, **options):
        response, _ = self.kernel.cell(code, **options)
        self.assertTrue(response["ok"], response.get("error"))
        self.assertNotIn("kernelError", response)
        self.assertNotIn("bridgeError", response)
        return response

    def expired(self, code, **options):
        started = time.monotonic()
        response, streams = self.kernel.cell(code, softTimeoutSecs=1, **options)
        self.assertLess(time.monotonic() - started, 8)
        self.assertFalse(response["ok"])
        self.assertTrue(response["softTimeout"])
        self.assertIn("EVAL_SOFT_TIMEOUT", response["error"])
        self.assertNotIn("kernelError", response)
        self.assertNotIn("bridgeError", response)
        return response, streams

    def test_busy_python_loop_retains_namespace_and_same_process(self):
        self.good("import os\nkeep = 42\npid = os.getpid()")
        self.expired("steps = 0\nwhile True:\n    steps += 1")
        after = self.good("[keep, steps > 0, os.getpid() == pid]")
        self.assertEqual(after["result"], "[42, True, True]")

    def test_synchronous_sleep_is_interrupted_and_finally_runs(self):
        self.expired("import time\ncleaned = False\ntry:\n    time.sleep(60)\n    unreachable = True\nfinally:\n    cleaned = True")
        self.assertEqual(self.good("[cleaned, 'unreachable' in globals()]")["result"], "[True, False]")

    def test_finite_work_beyond_the_soft_deadline_cannot_report_success(self):
        self.expired("import time\ntime.sleep(1.2)\n'completed too late'")

    def test_awaited_child_tasks_settle_under_the_same_soft_deadline(self):
        self.expired("""import asyncio
async def compute():
    while True:
        pass
async def wait():
    await asyncio.sleep(60)
children = [asyncio.create_task(compute()), asyncio.create_task(wait())]
await asyncio.gather(*children)
""")
        self.assertEqual(self.good("all(task.done() for task in children)")["result"], "True")

    def test_top_level_await_settles_root_and_preserves_contextvars(self):
        self.good("import asyncio, contextvars\nvalue = contextvars.ContextVar('kept')\nvalue.set(42)")
        self.expired("root = asyncio.current_task()\ncleaned = []\ntry:\n    await asyncio.sleep(60)\nfinally:\n    cleaned.append(1)")
        self.assertEqual(self.good("[root.done(), value.get(), cleaned]")["result"], "[True, 42, [1]]")

    def test_caught_soft_stop_is_not_success_and_cannot_dispatch_more_tools(self):
        self.expired("""try:
    while True:
        pass
except BaseException:
    try:
        tool.read('must-not-dispatch')
    except Exception as error:
        refused = str(error)
42
""")
        self.assertEqual(self.kernel.calls, [])
        self.assertIn("EVAL_SOFT_TIMEOUT", self.good("refused")["result"])
        self.assertIn("value:read", self.good("tool.read('current.txt')")["result"])

    def test_async_cancellation_suppression_still_reports_soft_timeout(self):
        self.expired("""import asyncio
root = asyncio.current_task()
try:
    await asyncio.sleep(60)
except asyncio.CancelledError:
    root.uncancel()
    cleaned = 42
cleaned
""")
        self.assertEqual(self.good("[root.done(), cleaned]")["result"], "[True, 42]")

    def test_early_completion_and_late_alarm_do_not_interrupt_a_new_cell(self):
        self.good("keep = 42", softTimeoutSecs=1)
        # Same process, now idle: a queued signal must not kill it.
        os.kill(self.kernel.process.pid, signal.SIGALRM)
        self.good("import time\ntime.sleep(1.2)\nkeep")
        self.expired("while True:\n    pass")
        self.assertEqual(self.good("keep")["result"], "42")

    def test_custom_signal_handler_is_not_overwritten_or_code_queued(self):
        self.good("import signal\nhandler = lambda *_: None\nsignal.signal(signal.SIGALRM, handler)")
        refused, _ = self.kernel.cell("unexpected = True", softTimeoutSecs=1)
        self.assertFalse(refused["ok"])
        self.assertIn("EVAL_SOFT_TIMEOUT_BUSY", refused["error"])
        self.assertEqual(self.good("[signal.getsignal(signal.SIGALRM) is handler, 'unexpected' in globals()]")["result"], "[True, False]")

    def test_existing_timer_is_not_canceled_by_soft_deadline_preflight(self):
        self.good("import signal\nsignal.signal(signal.SIGALRM, lambda *_: None)\nsignal.setitimer(signal.ITIMER_REAL, 60)")
        refused, _ = self.kernel.cell("unexpected = True", softTimeoutSecs=1)
        self.assertFalse(refused["ok"])
        self.assertIn("EVAL_SOFT_TIMEOUT_BUSY", refused["error"])
        self.assertEqual(self.good("[signal.getitimer(signal.ITIMER_REAL)[0] > 50, 'unexpected' in globals()]")["result"], "[True, False]")

    def test_blocked_alarm_is_refused_without_changing_the_signal_mask(self):
        self.good("import signal\nsignal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGALRM})")
        refused, _ = self.kernel.cell("unexpected = True", softTimeoutSecs=1)
        self.assertFalse(refused["ok"])
        self.assertIn("EVAL_SOFT_TIMEOUT_BLOCKED", refused["error"])
        self.assertEqual(self.good("[signal.SIGALRM in signal.pthread_sigmask(signal.SIG_BLOCK, []), 'unexpected' in globals()]")["result"], "[True, False]")

    def test_invalid_soft_budget_does_not_run_or_queue_code(self):
        for value in [0, -1, True, "1", 1.5, 600]:
            with self.subTest(value=value):
                refused, _ = self.kernel.cell("unexpected = True", softTimeoutSecs=value)
                self.assertFalse(refused["ok"])
                self.assertIn("EVAL_INPUT", refused["error"])
                self.assertNotIn("softTimeout", refused)
        self.assertEqual(self.good("'unexpected' in globals()")["result"], "False")

    def test_slow_synchronous_bridge_drains_before_native_retirement(self):
        def delayed(frame):
            time.sleep(1.2)
            return {"bridge_result": {"call":frame["bridge"]["call"], "ok":True, "content":"late"}}
        response, _ = self.kernel.cell("tool.read('slow.txt')", reply=delayed, softTimeoutSecs=1)
        self.assertFalse(response["ok"])
        self.assertTrue(response["softTimeout"])
        self.assertNotIn("bridgeError", response)
        self.assertIn("reply drained", response["kernelError"])
        self.assertEqual(len(self.kernel.calls), 1)

    def test_soft_expiry_drains_async_reply_without_dispatching_queued_work(self):
        def delayed(frame):
            time.sleep(1.2)
            return {"bridge_result": {"call":frame["bridge"]["call"], "ok":True, "content":"late"}}
        response, streams = self.kernel.cell("""import asyncio
async def local_work():
    await asyncio.sleep(0.05)
    print('async-loop-still-responsive')
await asyncio.gather(
    tool.call('read', {'path':'slow.txt'}),
    tool.call('read', {'path':'must-not-dispatch.txt'}),
    local_work(),
)
""", reply=delayed, softTimeoutSecs=1)
        self.assertFalse(response["ok"])
        self.assertTrue(response["softTimeout"])
        self.assertIn("reply drained", response["kernelError"])
        self.assertNotIn("bridgeError", response)
        self.assertEqual(len(self.kernel.calls), 1)
        self.assertEqual(self.kernel.calls[0]["bridge"]["input"]["path"], "slow.txt")
        self.assertTrue(any(frame["stream"].get("text") == "async-loop-still-responsive" for frame in streams))

    def test_successful_async_bridge_then_compute_timeout_can_retain_state(self):
        response, _ = self.expired("answer = await tool.call('read', {'path':'early.txt'})\nwhile True:\n    pass")
        self.assertEqual(len(self.kernel.calls), 1)
        self.assertNotIn("kernelError", response)
        self.assertEqual(self.good("answer")["result"], "'value:read'")

    def test_soft_expiry_during_bridge_finalization_never_leaves_a_reader(self):
        def delayed(frame):
            time.sleep(1.2)
            return {"bridge_result": {"call":frame["bridge"]["call"], "ok":True, "content":"late"}}
        response, _ = self.kernel.cell("""import asyncio
pending = asyncio.create_task(tool.call('read', {'path':'slow.txt'}))
await asyncio.sleep(0)
42
""", reply=delayed, softTimeoutSecs=1)
        self.assertFalse(response["ok"])
        self.assertTrue(response["softTimeout"])
        self.assertIn("kernelError", response)
        self.assertEqual(len(self.kernel.calls), 1)
        self.kernel.process.wait(timeout=5)

    def test_unsettled_root_is_never_advertised_as_reusable(self):
        response, _ = self.kernel.cell("import asyncio\nasyncio.get_running_loop().stop()\nawait asyncio.sleep(60)", softTimeoutSecs=1)
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_KERNEL_INCOMPLETE", response["kernelError"])

    def test_kernel_managed_package_operation_prevents_soft_reuse(self):
        probe = subprocess.run([sys.executable, "-I", "-m", "pip", "--version"],
                               capture_output=True, timeout=10, check=False)
        if probe.returncode != 0:
            self.skipTest("pip not installed for the test interpreter")
        # Real pip process, no installation or network: --help returns before
        # this deliberate loop. Any soft-timed-out cell which entered package
        # management requires native process-tree retirement conservatively.
        response, _ = self.kernel.cell("""install = tool.read.__globals__['_install_python_packages']
await install(['--help'])
while True:
    pass
""", softTimeoutSecs=1)
        self.assertFalse(response["ok"])
        self.assertTrue(response["softTimeout"])
        self.assertIn("process-tree retirement", response["kernelError"])

    def test_timeout_preserves_accepted_console_and_rich_output(self):
        response, streams = self.expired("print('accepted-before-timeout')\ndisplay({'answer':42})\nwhile True:\n    pass")
        self.assertEqual(response["displays"][0]["data"], {"answer":42})
        self.assertTrue(any(frame["stream"].get("text") == "accepted-before-timeout" for frame in streams))
        self.assertEqual(self.good("42")["result"], "42")

    def test_plot_cleanup_runs_after_soft_interruption(self):
        self.good("import matplotlib.pyplot as plt")
        response, _ = self.expired("figure, axes = plt.subplots(figsize=(1,1), dpi=30)\naxes.plot([0,1], [1,0])\nwhile True:\n    pass")
        self.assertEqual(len(response["displays"]), 1)
        self.assertEqual(response["displays"][0]["mimeType"], "image/png")
        self.assertEqual(self.good("[plt.get_fignums(), figure is not None]")["result"], "[[], True]")


if __name__ == "__main__":
    unittest.main(verbosity=2)
