"""Actual Python-kernel protocol regressions; not Rust-host acceptance.

The subprocess runs the shipped server. This harness speaks only its wire
protocol, recording every bridge request before returning a controlled reply.
Use PI_EVAL_KERNEL_SERVER to run a negative control against an older server.
"""

import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
import unittest

SERVER = Path(os.environ.get(
    "PI_EVAL_KERNEL_SERVER",
    str(Path(__file__).resolve().parents[1] / "src/eval/py_kernel_server.py"),
))


class Kernel:
    def __init__(self):
        self.process = subprocess.Popen(
            [sys.executable, "-u", str(SERVER)], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            encoding="utf-8", bufsize=1,
        )
        self.frames = queue.Queue()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        self.next_id = 0
        self.calls = []

    def _read(self):
        try:
            for line in self.process.stdout:
                self.frames.put(json.loads(line))
        except BaseException as error:
            self.frames.put(error)
        finally:
            self.frames.put(None)

    def send(self, frame):
        self.process.stdin.write(json.dumps(frame, ensure_ascii=False) + "\n")
        self.process.stdin.flush()

    def cell(self, code, reply=None):
        self.next_id += 1
        self.send({"id": self.next_id, "code": code})
        streams = []
        while True:
            try:
                frame = self.frames.get(timeout=10)
            except queue.Empty as error:
                raise AssertionError("kernel did not finish the cell") from error
            if frame is None or isinstance(frame, BaseException):
                raise AssertionError(f"kernel protocol stopped: {frame!r}")
            if "bridge" in frame:
                self.calls.append(frame)
                call = frame["bridge"]
                response = reply(frame) if reply else {
                    "bridge_result": {"call": call["call"], "ok": True,
                                      "content": f"value:{call['tool']}"},
                }
                self.send(response)
            elif "stream" in frame:
                streams.append(frame)
            else:
                if frame.get("id") != self.next_id:
                    raise AssertionError(f"wrong final cell: {frame!r}")
                return frame, streams

    def close(self):
        self.process.kill()
        self.process.wait(timeout=5)
        self.reader.join(timeout=5)
        self.process.stdin.close()
        self.process.stdout.close()
        self.process.stderr.close()


class BridgeTests(unittest.TestCase):
    def setUp(self):
        self.kernel = Kernel()
        self.addCleanup(self.kernel.close)

    def run_cell(self, code, reply=None):
        response, streams = self.kernel.cell(code, reply)
        self.assertTrue(response["ok"], response.get("error"))
        self.assertNotIn("bridgeError", response)
        return response, streams

    def test_sync_helpers_and_awaited_calls_keep_cell_and_exchange_identity(self):
        self.run_cell("saved = tool.read; result = saved('one')")
        response, _ = self.run_cell("[saved('two'), tool.grep('needle'), tool.find('*.rs'), tool.ls(), await tool.call('read', {'path':'three'})]")
        self.assertEqual([call["id"] for call in self.kernel.calls], [1, 2, 2, 2, 2, 2])
        self.assertEqual([call["bridge"]["call"] for call in self.kernel.calls], list(range(1, 7)))
        self.assertIn("value:read", response["result"])

    def test_live_gather_remains_serialized_and_keeps_argument_values(self):
        response, _ = self.run_cell("import asyncio\nawait asyncio.gather(tool.call('read', {'path':'界.txt'}), tool.call('ls', {'path':'.'}))")
        self.assertIn("value:ls", response["result"])
        self.assertEqual(self.kernel.calls[0]["bridge"]["input"]["path"], "界.txt")
        self.assertEqual([call["bridge"]["call"] for call in self.kernel.calls], [1, 2])

    def test_tool_refusal_is_catchable_without_poisoning_or_losing_variables(self):
        def denied(frame):
            return {"bridge_result": {"call": frame["bridge"]["call"],
                                      "ok": False, "error": "EVAL_BRIDGE_DENIED: test refusal"}}
        response, _ = self.run_cell("keep = 42\ntry:\n    tool.read('denied')\nexcept Exception as error:\n    refusal = str(error)\nrefusal", denied)
        self.assertIn("EVAL_BRIDGE_DENIED", response["result"])
        response, _ = self.run_cell("[keep, tool.read('allowed')]")
        self.assertIn("42", response["result"])
        self.assertIn("value:read", response["result"])

    def seed_late_task(self, fail=False):
        code = """import asyncio
ready = asyncio.Event()
verdicts = []
async def late():
    await ready.wait()
    try:
        verdicts.append(await tool.call('read', {'path':'old-cell.txt'}))
    except Exception as error:
        verdicts.append(str(error))
old_task = asyncio.create_task(late())
await asyncio.sleep(0)
"""
        if fail:
            code += "raise RuntimeError('original failure')\n"
        response, _ = self.kernel.cell(code)
        self.assertEqual(response["ok"], not fail)

    def test_retained_task_cannot_borrow_a_later_cells_tool_authority(self):
        self.seed_late_task()
        response, _ = self.run_cell("ready.set()\nawait old_task\nverdicts[0]")
        self.assertIn("EVAL_BRIDGE_CLOSED", response["result"])
        self.assertEqual(self.kernel.calls, [])
        self.run_cell("tool.read('current-cell.txt')")
        self.assertEqual(self.kernel.calls[0]["id"], 3)

    def test_failed_cells_retained_tasks_are_also_revoked(self):
        self.seed_late_task(fail=True)
        response, _ = self.run_cell("ready.set()\nawait old_task\nverdicts[0]")
        self.assertIn("EVAL_BRIDGE_CLOSED", response["result"])
        self.assertEqual(self.kernel.calls, [])

    def test_saved_context_cannot_restore_an_expired_bridge_lease(self):
        self.run_cell("import contextvars\nold_context = contextvars.copy_context()")
        response, _ = self.run_cell("try:\n    old_context.run(tool.read, 'old.txt')\nexcept Exception as error:\n    verdict = str(error)\nverdict")
        self.assertIn("EVAL_BRIDGE_CLOSED", response["result"])
        self.assertEqual(self.kernel.calls, [])

    def test_context_copied_worker_cannot_consume_protocol_stdin(self):
        response, _ = self.run_cell("""import asyncio
try:
    await asyncio.to_thread(tool.read, 'thread.txt')
except Exception as error:
    verdict = str(error)
verdict
""")
        self.assertIn("EVAL_BRIDGE_THREAD", response["result"])
        self.assertEqual(self.kernel.calls, [])
        self.run_cell("tool.read('main.txt')")

    def test_unattributed_worker_cannot_emit_a_tool_request(self):
        response, _ = self.run_cell("""import threading
verdicts = []
def worker():
    try:
        verdicts.append(tool.read('thread.txt'))
    except Exception as error:
        verdicts.append(str(error))
thread = threading.Thread(target=worker)
thread.start()
thread.join()
verdicts[0]
""")
        self.assertIn("EVAL_BRIDGE_CLOSED", response["result"])
        self.assertEqual(self.kernel.calls, [])

    def test_non_json_arguments_are_refused_before_host_effects(self):
        response, _ = self.run_cell("""attempts = []
class Sneaky(dict):
    def items(self):
        attempts.append('encoder ran')
        return super().items()
for arguments in [Sneaky(path='x'), {'x':float('nan')}, {'x':object()}, {'x':1 << 100}]:
    try:
        await tool.call('read', arguments)
    except Exception as error:
        pass
attempts
""")
        self.assertEqual(response["result"], "[]")
        self.assertEqual(self.kernel.calls, [])
        self.run_cell("tool.read('okay')")
        self.assertEqual(self.kernel.calls[0]["bridge"]["call"], 1)

    def test_oversized_request_is_refused_without_poisoning_the_channel(self):
        response, _ = self.run_cell("try:\n    tool.read('x' * (1024 * 1024))\nexcept Exception as error:\n    verdict = str(error)\nverdict")
        self.assertIn("EVAL_BRIDGE_LIMIT", response["result"])
        self.assertEqual(self.kernel.calls, [])
        self.run_cell("tool.read('okay')")
        self.assertEqual(self.kernel.calls[0]["bridge"]["call"], 1)

    def test_reentrant_exchange_is_refused_without_waiting_on_the_cell_thread(self):
        response, _ = self.run_cell("""lock = tool.read.__globals__['_BRIDGE_EXCHANGE']
lock.acquire()
try:
    try:
        tool.read('recursive.txt')
    except Exception as error:
        verdict = str(error)
finally:
    lock.release()
verdict
""")
        self.assertIn("EVAL_BRIDGE_BUSY", response["result"])
        self.assertEqual(self.kernel.calls, [])
        self.run_cell("tool.read('okay')")

    def assert_bad_reply_poisoned(self, reply):
        response, _ = self.kernel.cell("""caught = []
for _ in range(2):
    try:
        tool.read('x')
    except Exception as error:
        caught.append(str(error))
42
""", reply)
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_BRIDGE_PROTOCOL", response["bridgeError"])
        self.assertEqual(len(self.kernel.calls), 1, "poisoned channel issued another request")
        self.assertIn("EVAL_BRIDGE_PROTOCOL", response["error"])

    def test_wrong_reply_identity_cannot_be_swallowed_into_success(self):
        self.assert_bad_reply_poisoned(lambda frame: {"bridge_result": {
            "call": frame["bridge"]["call"] + 1, "ok": True, "content": "wrong",
        }})

    def test_boolean_call_id_is_not_the_integer_one(self):
        self.assert_bad_reply_poisoned(lambda _: {"bridge_result": {
            "call": True, "ok": True, "content": "wrong",
        }})

    def test_non_boolean_status_does_not_become_success(self):
        self.assert_bad_reply_poisoned(lambda frame: {"bridge_result": {
            "call": frame["bridge"]["call"], "ok": "false", "content": "wrong",
        }})

    def test_non_text_tool_output_poisons_the_protocol(self):
        self.assert_bad_reply_poisoned(lambda frame: {"bridge_result": {
            "call": frame["bridge"]["call"], "ok": True, "content": {"not":"text"},
        }})

    def test_unexpected_reply_envelope_poisons_the_protocol(self):
        self.assert_bad_reply_poisoned(lambda _: {"id": 2, "code": "raise Exception('not a reply')"})

    def test_prior_streamed_output_survives_a_corrupt_tool_reply(self):
        response, streams = self.kernel.cell("print('before')\ndisplay({'accepted':42})\ntool.read('bad')", lambda _: {})
        self.assertFalse(response["ok"])
        self.assertIn("bridgeError", response)
        self.assertEqual(response["displays"][0]["data"], {"accepted":42})
        self.assertTrue(any(frame["stream"].get("text") == "before" for frame in streams))


if __name__ == "__main__":
    unittest.main(verbosity=2)
