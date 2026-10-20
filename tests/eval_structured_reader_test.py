"""Diagnostics for the complete embedded Python kernel with a controlled host.

This runs the production script in real subprocesses, including its event loop,
private stdio protocol, reply reader, cancellation and cell finalization. Host
replies are fixtures, not Rust tool execution or a substitute for DSR acceptance.
"""

import ast
import json
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "src/eval/py_kernel_server.py"
SCHEMA = "pi.eval.tool_result.v1"


def envelope(details=None, *, failed=False):
    return {"schema": SCHEMA, "content": [{"type": "text", "text": "payload"}],
            "details": details, "isError": failed}


class Kernel:
    def __init__(self):
        self.directory = tempfile.TemporaryDirectory(prefix="pi-structured-protocol-")
        self.process = subprocess.Popen(
            [sys.executable, "-u", str(SCRIPT)], cwd=self.directory.name,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", bufsize=1,
        )
        self.frames = queue.Queue()
        self.calls = []
        self.next_cell = 0

        def receive():
            try:
                for line in self.process.stdout:
                    self.frames.put(json.loads(line))
            except Exception as error:
                self.frames.put(error)
            finally:
                self.frames.put(None)

        self.reader = threading.Thread(target=receive, daemon=True)
        self.reader.start()

    def send(self, value):
        self.process.stdin.write(json.dumps(value, ensure_ascii=False) + "\n")
        self.process.stdin.flush()

    def reply(self, frame, content=None, *, error=None):
        result = {"call": frame["bridge"]["call"], "ok": error is None}
        result["content" if error is None else "error"] = content if error is None else error
        self.send({"bridge_result": result})

    def cell(self, code, bridge=None, observe=None):
        self.next_cell += 1
        cell_id = self.next_cell
        self.send({"id": cell_id, "code": code})
        deadline = time.monotonic() + 10
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError("kernel did not settle within the diagnostic deadline")
            try:
                frame = self.frames.get(timeout=remaining)
            except queue.Empty as error:
                raise AssertionError("kernel stalled while awaiting a protocol frame") from error
            if frame is None:
                raise AssertionError("kernel exited before its terminal response")
            if isinstance(frame, Exception):
                raise frame
            if frame.get("id") != cell_id:
                raise AssertionError(f"cross-cell frame: expected {cell_id}, got {frame!r}")
            if "bridge" in frame:
                self.calls.append(frame)
                if bridge is None:
                    raise AssertionError(f"unexpected bridge dispatch: {frame!r}")
                bridge(frame)
            elif "stream" in frame:
                if observe is not None:
                    observe(frame)
            else:
                return frame

    def close(self):
        try:
            if self.process.poll() is None:
                self.process.stdin.close()
                try:
                    self.process.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.process.terminate()
                    try:
                        self.process.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        self.process.kill()
                        self.process.wait(timeout=2)
        finally:
            self.reader.join(timeout=2)
            for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
                stream.close()
            self.directory.cleanup()


class StructuredReaderTests(unittest.TestCase):
    def setUp(self):
        self.kernel = Kernel()
        self.addCleanup(self.kernel.close)

    def value(self, response):
        self.assertTrue(response["ok"], response.get("error"))
        return ast.literal_eval(response["result"])

    def reply_with(self, value):
        return lambda frame: self.kernel.reply(frame, json.dumps(value, ensure_ascii=False))

    def assert_next_cell_works(self):
        response = self.kernel.cell("await tool.call('read', {'path': 'next'})",
                                    lambda frame: self.kernel.reply(frame, "next-cell"))
        self.assertEqual(self.value(response), "next-cell")

    def test_structured_result_preserves_all_details_shapes(self):
        for details in (None, 42, True, [1, "x"], {"nested": {"value": "é"}}):
            with self.subTest(details=details):
                expected = envelope(details)
                response = self.kernel.cell("await tool.result('read', {'path': 'a'})",
                                            self.reply_with(expected))
                self.assertEqual(self.value(response), expected)
                self.assertEqual(self.kernel.calls[-1]["bridge"]["tool"], "__pi_tool_result")
                self.assertEqual(self.kernel.calls[-1]["bridge"]["input"],
                                 {"name": "read", "args": {"path": "a"}})

    def test_native_failure_and_image_blocks_are_returned_without_display(self):
        expected = envelope({"reason": "native-failure"}, failed=True)
        expected["content"].append({"type": "image", "data": "aW1hZ2U=", "mimeType": "image/png"})
        frames = []
        response = self.kernel.cell("await tool.result('read', {})", self.reply_with(expected), frames.append)
        self.assertEqual(self.value(response), expected)
        self.assertEqual(response["displays"], [])
        self.assertEqual(frames, [])

    def test_existing_sync_and_async_text_calls_are_unchanged(self):
        for code in ("tool.read('a')", "await tool.call('read', {'path':'a'})"):
            response = self.kernel.cell(code, lambda frame: self.kernel.reply(frame, "plain text"))
            self.assertEqual(self.value(response), "plain text")
            self.assertEqual(self.kernel.calls[-1]["bridge"]["tool"], "read")

    def test_result_objects_survive_later_cells(self):
        response = self.kernel.cell("saved = await tool.result('read', {}); saved['details']",
                                    self.reply_with(envelope({"count": 41})))
        self.assertEqual(self.value(response), {"count": 41})
        response = self.kernel.cell("saved['details']['count'] += 1; saved['details']['count']")
        self.assertEqual(self.value(response), 42)
        self.assertEqual(len(self.kernel.calls), 1)

    def launch_agent(self, variable="saved", job_id="job-native"):
        def bridge(frame):
            self.assertEqual(frame["bridge"]["input"], {
                "name": "subagent", "args": {
                    "agent": "scout", "task": "inspect the code", "async": True,
                },
            })
            self.kernel.reply(frame, json.dumps(envelope({
                "schema": "pi.subagent.background.v1", "jobId": job_id,
            })))
        response = self.kernel.cell(
            f"{variable} = await agent('inspect the code', agent='scout'); {variable}.id", bridge,
        )
        self.assertEqual(self.value(response), job_id)

    def test_agent_handle_survives_cells_and_uses_native_result_without_stored_authority(self):
        self.launch_agent()
        native = {"content": [{"type": "text", "text": "completed"}],
                  "details": {"results": [{"data": {"count": 7}}]}}
        snapshot = {"schema": "pi.host_job.v1", "id": "job-native", "status": "exited", "result": native}
        response = self.kernel.cell("await saved", self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response), native)
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
            "name": "jobs", "args": {"action": "wait", "jobId": "job-native", "timeoutMs": 30000},
        })
        response = self.kernel.cell("await saved.status()",
                                    lambda frame: self.kernel.reply(frame, error="PI_JOBS_UNKNOWN_ID"))
        self.assertFalse(response["ok"])
        self.assertIn("PI_JOBS_UNKNOWN_ID", response["error"])

    def test_agent_options_cannot_override_operation_or_select_foreground(self):
        for option in ("op", "async", "task", "model"):
            with self.subTest(option=option):
                response = self.kernel.cell(f"await agent('inspect', **{{{option!r}: 'override'}})")
                self.assertFalse(response["ok"])
                self.assertIn("unknown option", response["error"])
        self.assertEqual(self.kernel.calls, [])

    def test_agent_wait_timeout_does_not_cancel_or_replay_the_job(self):
        self.launch_agent()
        snapshot = {"schema": "pi.host_job.v1", "id": "job-native", "status": "running"}
        response = self.kernel.cell("await saved.wait(0)", self.reply_with(envelope(snapshot)))
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_AGENT_TIMEOUT", response["error"])
        self.assertEqual(len(self.kernel.calls), 2)
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"]["args"]["action"], "wait")

    def test_wait_keeps_input_order_and_can_return_failed_native_results(self):
        self.launch_agent("first", "job-first")
        self.launch_agent("second", "job-second")
        def bridge(frame):
            job_id = frame["bridge"]["input"]["args"]["jobId"]
            failed = job_id == "job-second"
            native = {"content": [], "details": {"id": job_id}, "isError": failed}
            self.kernel.reply(frame, json.dumps(envelope({"schema": "pi.host_job.v1",
                "id": job_id, "status": "failed" if failed else "exited", "result": native})))
        response = self.kernel.cell("await wait([second, first], timeout=10, raise_errors=False)", bridge)
        results = self.value(response)
        self.assertEqual([result["details"]["id"] for result in results], ["job-second", "job-first"])
        self.assertTrue(results[0]["isError"])
        self.assertFalse(results[1]["isError"])

    def test_malformed_arguments_never_dispatch(self):
        for args in ("4, {}", "'', {}", "'read', None", "'read', []", "'é' * 65, {}",
                     "'read', {'bad': object()}", "'read', {'not-finite': float('nan')}"):
            with self.subTest(args=args):
                response = self.kernel.cell(f"await tool.result({args})")
                self.assertFalse(response["ok"])
                self.assertEqual(self.kernel.calls, [])
        self.assert_next_cell_works()

    def test_policy_denial_rejects_once_without_fallback(self):
        response = self.kernel.cell("await tool.result('bash', {'command':'true'})",
                                    lambda frame: self.kernel.reply(frame, error="EVAL_BRIDGE_DENIED"))
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_BRIDGE_DENIED", response["error"])
        self.assertEqual(len(self.kernel.calls), 1)
        self.assert_next_cell_works()

    def test_invalid_envelopes_do_not_poison_a_drained_transport(self):
        missing_details = envelope()
        del missing_details["details"]
        values = ("not-json", "null", "[]", "{}", json.dumps(missing_details),
                  json.dumps({**envelope(), "schema": "wrong"}),
                  json.dumps({**envelope(), "isError": 0}),
                  json.dumps({**envelope(), "content": "text"}),
                  json.dumps(envelope(float("nan"))), json.dumps(envelope(float("inf"))))
        for encoded in values:
            with self.subTest(encoded=encoded):
                response = self.kernel.cell("await tool.result('read', {})",
                                            lambda frame: self.kernel.reply(frame, encoded))
                self.assertFalse(response["ok"])
                self.assertIn("EVAL_BRIDGE_PROTOCOL", response["error"])
                self.assertNotIn("bridgeError", response)
        self.assertEqual(len(self.kernel.calls), len(values))
        self.assert_next_cell_works()

    def test_utf8_result_budget_is_not_a_character_count(self):
        encoded = json.dumps(envelope("é" * (2 * 1024 * 1024)), ensure_ascii=False)
        self.assertLess(len(encoded), 4 * 1024 * 1024)
        self.assertGreater(len(encoded.encode("utf-8")), 4 * 1024 * 1024)
        response = self.kernel.cell("await tool.result('read', {})",
                                    lambda frame: self.kernel.reply(frame, encoded))
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_BRIDGE_LIMIT", response["error"])
        self.assert_next_cell_works()

    def test_mixed_concurrent_calls_keep_their_result_modes(self):
        def bridge(frame):
            if frame["bridge"]["tool"] == "__pi_tool_result":
                self.kernel.reply(frame, json.dumps(envelope({"second": 2})))
            else:
                self.kernel.reply(frame, "first")
        response = self.kernel.cell(
            "import asyncio\nawait asyncio.gather(tool.call('read', {}), tool.result('read', {}))", bridge)
        self.assertEqual(self.value(response), ["first", envelope({"second": 2})])
        self.assertEqual(len(self.kernel.calls), 2)

    def cancellation_exchange(self, code):
        pending = []
        released = []

        def bridge(frame):
            pending.append(frame)
            self.assertEqual(len(pending), 1, "queued/cancelled call was dispatched")

        def observe(frame):
            if "release-reply" in frame["stream"].get("text", ""):
                self.assertEqual(len(pending), 1)
                self.kernel.reply(pending[0], json.dumps(envelope({"settled": True})))
                released.append(True)

        response = self.kernel.cell(code, bridge, observe)
        self.assertEqual(released, [True])
        self.assertEqual(len(self.kernel.calls), 1)
        self.assert_next_cell_works()
        return response

    def test_dispatched_cancellation_drains_once_before_next_cell(self):
        response = self.cancellation_exchange("""
import asyncio
running = asyncio.create_task(tool.result('read', {}))
await asyncio.sleep(0.02)
running.cancel()
print('release-reply')
try:
    await running
    verdict = 'unexpected success'
except asyncio.CancelledError:
    verdict = 'cancelled'
verdict
""")
        self.assertEqual(self.value(response), "cancelled")

    def test_queued_cancellation_sends_nothing(self):
        response = self.cancellation_exchange("""
import asyncio
running = asyncio.create_task(tool.result('read', {'path':'running'}))
queued = asyncio.create_task(tool.result('read', {'path':'never-dispatch'}))
await asyncio.sleep(0.02)
queued.cancel()
try:
    await queued
except asyncio.CancelledError:
    pass
print('release-reply')
(await running)['details']
""")
        self.assertEqual(self.value(response), {"settled": True})

    def test_cell_end_drains_unawaited_work_and_reports_failure(self):
        response = self.cancellation_exchange("""
import asyncio
unfinished = asyncio.create_task(tool.result('read', {}))
await asyncio.sleep(0.02)
print('release-reply')
'body returned'
""")
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_BRIDGE_PENDING", response["error"])

    def test_worker_context_cannot_steal_bridge_stdin(self):
        response = self.kernel.cell("""
import asyncio
await asyncio.to_thread(lambda: asyncio.run(tool.result('read', {})))
""")
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_BRIDGE_THREAD", response["error"])
        self.assertEqual(self.kernel.calls, [])
        self.assert_next_cell_works()


if __name__ == "__main__":
    unittest.main(verbosity=2)
