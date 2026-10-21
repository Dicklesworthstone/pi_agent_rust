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


def worker_snapshot(worker_id="worker-native", turn_id=1, status="running", result=None):
    return {
        "schema": "pi.subagent.worker.v1", "workerId": worker_id,
        "jobId": f"carrier-{worker_id}", "agent": "scout",
        "status": "idle" if status == "completed" else "closed" if status == "cancelled" else "running",
        "turnId": turn_id, "turn": {"id": turn_id, "status": status, "result": result},
    }


def turn_result(turn_id):
    return {"content": [{"type": "text", "text": f"turn {turn_id}"}], "details": {"turn": turn_id}}


class PoolHost:
    def __init__(self, kernel):
        self.kernel = kernel
        self.pools = {}
        self.operations = []

    def snapshot(self, pool):
        counts = {status: 0 for status in ("queued", "running", "completed", "failed", "cancelled")}
        for item in pool["items"]:
            counts[item["status"]] += 1
        return {"schema": "pi.subagent.pool.v1", "poolId": pool["id"], "jobId": pool["jobId"],
                "agent": pool["agent"], "status": pool["status"], "sealed": pool["sealed"],
                "concurrency": pool["concurrency"], "admittedItems": len(pool["items"]),
                "queuedItems": counts["queued"], "runningItems": counts["running"],
                "completedItems": counts["completed"], "failedItems": counts["failed"] + counts["cancelled"],
                "remainingMs": 900000, "items": [
                    {"id": item["id"], "status": item["status"],
                     "summary": f"item {item['id']}" if item["result"] is not None else "",
                     "resultRef": {"poolId": pool["id"], "itemId": item["id"]}}
                    for item in pool["items"]]}

    @staticmethod
    def complete(pool, item_id, result=None):
        result = turn_result(item_id) if result is None else result
        pool["items"][item_id - 1].update(status="failed" if result.get("isError") else "completed", result=result)

    def apply(self, frame):
        request = frame["bridge"]["input"]
        args = request["args"]
        if request["name"] == "jobs":
            self.operations.append(f"jobs:{args['action']}")
            pool = next(pool for pool in self.pools.values() if pool["jobId"] == args["jobId"])
            snapshot = {"schema": "pi.host_job.v1", "id": pool["jobId"], "status": "running"}
            if pool["sealed"]:
                for item in pool["items"]:
                    if item["result"] is None:
                        self.complete(pool, item["id"])
                pool["status"] = "failed" if any(item["status"] == "failed" for item in pool["items"]) else "completed"
                snapshot.update(status="exited", result={
                    "content": [{"type": "text", "text": "aggregate completed"}],
                    "details": self.snapshot(pool), "isError": pool["status"] == "failed",
                })
            return snapshot
        if request["name"] != "subagent":
            raise AssertionError("pool must use native subagent operations")
        operation = args["operation"]
        self.operations.append(operation)
        if operation == "pool_start":
            pool_id = f"pool-native-{len(self.pools) + 1}"
            pool = {"id": pool_id, "jobId": f"carrier-{pool_id}", "agent": args["agent"],
                    "concurrency": args["concurrency"], "status": "open", "sealed": False, "items": []}
            self.pools[pool_id] = pool
        else:
            pool = self.pools[args["poolId"]]
        admitted = []
        if operation in ("pool_start", "pool_push"):
            if pool["sealed"]:
                raise AssertionError("guest must not admit work after sealing")
            for task in args["tasks"]:
                item_id = len(pool["items"]) + 1
                pool["items"].append({"id": item_id, "task": task, "result": None,
                                      "status": "running" if item_id <= pool["concurrency"] else "queued"})
                admitted.append(item_id)
        elif operation == "pool_finish":
            pool["sealed"] = True
            pool["status"] = "draining" if any(item["result"] is None for item in pool["items"]) else "completed"
        elif operation == "pool_close":
            pool.update(sealed=True, status="cancelled")
            for item in pool["items"]:
                if item["result"] is None:
                    item.update(status="cancelled", result={"content": [], "details": {"cancelled": True}, "isError": True})
        elif operation not in ("pool_status", "pool_result", "pool_wait"):
            raise AssertionError(f"unexpected operation {operation}")
        snapshot = self.snapshot(pool)
        if operation in ("pool_start", "pool_push"):
            snapshot["admittedItemIds"] = admitted
        if "itemId" in args:
            item = pool["items"][args["itemId"] - 1]
            snapshot.update(itemId=item["id"], item={"id": item["id"], "status": item["status"], "result": item["result"]})
        return snapshot

    def __call__(self, frame):
        self.kernel.reply(frame, json.dumps(envelope(self.apply(frame))))


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
        for option in ("op", "operation", "async", "task", "model"):
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

    def launch_worker(self):
        response = self.kernel.cell(
            "worker = await agent('inspect the code', agent='scout', keep_alive=True, isolation='none'); worker.id",
            self.reply_with(envelope(worker_snapshot())),
        )
        self.assertEqual(self.value(response), "worker-native")
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
            "name": "subagent", "args": {"agent": "scout", "isolation": "none",
                "task": "inspect the code", "operation": "worker_start"},
        })

    def test_warm_worker_reuses_identity_across_cells_and_turns_keep_their_selection(self):
        self.launch_worker()
        response = self.kernel.cell("second = await worker.send('follow up'); (worker.turn_id, second.turn_id)",
                                    self.reply_with(envelope(worker_snapshot(turn_id=2, status="queued"))))
        self.assertEqual(self.value(response), (2, 2))
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
            "name": "subagent", "args": {"operation": "worker_send", "workerId": "worker-native", "task": "follow up"},
        })
        response = self.kernel.cell("third = await worker.send('another'); worker.turn_id",
                                    self.reply_with(envelope(worker_snapshot(turn_id=3, status="queued"))))
        self.assertEqual(self.value(response), 3)
        for expression, turn_id in (("await second", 2), ("await worker", 3)):
            response = self.kernel.cell(expression, self.reply_with(envelope(
                worker_snapshot(turn_id=turn_id, status="completed", result=turn_result(turn_id)))))
            self.assertEqual(self.value(response)["details"]["turn"], turn_id)
            self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
                "name": "subagent", "args": {"operation": "worker_wait", "workerId": "worker-native",
                    "turnId": turn_id, "timeoutMs": 30000},
            })

    def test_mixed_wait_accepts_jobs_workers_and_pinned_turns_with_one_deadline(self):
        self.launch_agent("job", "job-once")
        self.launch_worker()
        response = self.kernel.cell("second = await worker.send('second'); second.turn_id",
                                    self.reply_with(envelope(worker_snapshot(turn_id=2, status="queued"))))
        self.assertEqual(self.value(response), 2)
        response = self.kernel.cell("third = await worker.send('third'); worker.turn_id",
                                    self.reply_with(envelope(worker_snapshot(turn_id=3, status="queued"))))
        self.assertEqual(self.value(response), 3)
        requests = []
        def bridge(frame):
            request = frame["bridge"]["input"]
            requests.append(request)
            self.assertGreaterEqual(request["args"]["timeoutMs"], 0)
            self.assertLessEqual(request["args"]["timeoutMs"], 10000)
            if request["name"] == "jobs":
                snapshot = {"schema": "pi.host_job.v1", "id": "job-once", "status": "exited",
                            "result": turn_result(0)}
            else:
                turn_id = request["args"]["turnId"]
                native = {**turn_result(turn_id), "isError": turn_id == 2}
                snapshot = worker_snapshot(turn_id=turn_id,
                                           status="failed" if turn_id == 2 else "completed", result=native)
            self.kernel.reply(frame, json.dumps(envelope(snapshot)))
        response = self.kernel.cell("await wait([job, second, worker], timeout=10, raise_errors=False)", bridge)
        results = self.value(response)
        self.assertEqual([item["details"]["turn"] for item in results], [0, 2, 3])
        self.assertTrue(results[1]["isError"])
        self.assertEqual([request["name"] for request in requests], ["jobs", "subagent", "subagent"])
        self.assertGreaterEqual(requests[0]["args"]["timeoutMs"], requests[-1]["args"]["timeoutMs"])

    def test_wait_freezes_worker_selection_before_a_concurrent_send(self):
        self.launch_agent("job", "job-once")
        self.launch_worker()
        selected_turns = []
        def bridge(frame):
            request = frame["bridge"]["input"]
            if request["name"] == "jobs":
                snapshot = {"schema": "pi.host_job.v1", "id": "job-once", "status": "exited",
                            "result": turn_result(0)}
            elif request["args"]["operation"] == "worker_send":
                snapshot = worker_snapshot(turn_id=2, status="queued")
            else:
                selected_turns.append(request["args"]["turnId"])
                snapshot = worker_snapshot(status="completed", result=turn_result(1))
            self.kernel.reply(frame, json.dumps(envelope(snapshot)))
        response = self.kernel.cell("""
import asyncio
waiting = asyncio.create_task(wait([job, worker], timeout=10))
await asyncio.sleep(0)
sent = asyncio.create_task(worker.send('new turn'))
values = await waiting
await sent
([item['details']['turn'] for item in values], worker.turn_id)
""", bridge)
        self.assertEqual(self.value(response), ([0, 1], 2))
        self.assertEqual(selected_turns, [1])

    def test_worker_timeout_leaves_work_running_and_turn_cancel_closes_whole_worker(self):
        self.launch_worker()
        response = self.kernel.cell("await worker.wait(0)", self.reply_with(envelope(worker_snapshot())))
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_AGENT_TIMEOUT", response["error"])
        self.assertEqual(len(self.kernel.calls), 2)
        response = self.kernel.cell("turn = await worker.send('queued'); turn.turn_id",
                                    self.reply_with(envelope(worker_snapshot(turn_id=2, status="queued"))))
        self.assertEqual(self.value(response), 2)
        cancelled = {"content": [{"type": "text", "text": "cancelled"}], "details": None, "isError": True}
        for expression in ("await turn.cancel()", "await worker.close()"):
            response = self.kernel.cell(expression, self.reply_with(envelope(
                worker_snapshot(turn_id=2, status="cancelled", result=cancelled))))
            self.assertEqual(self.value(response)["status"], "closed")
            self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
                "name": "subagent", "args": {"operation": "worker_close", "workerId": "worker-native"},
            })

    def test_warm_options_and_empty_sends_fail_before_dispatch(self):
        for options in ({"keep_alive": 1}, {"keep_alive": "true"}, {"keep_alive": None},
                        {"keep_alive": True, "isolation": "worktree"},
                        {"keep_alive": True, "iso_apply": False},
                        {"keep_alive": True, "output_schema": {}},
                        {"keep_alive": True, "schema_mode": "strict"},
                        {"keep_alive": True, "op": "worker_close"},
                        {"keep_alive": True, "worker_id": "foreign"}):
            with self.subTest(options=options):
                response = self.kernel.cell(f"await agent('inspect', **{options!r})")
                self.assertFalse(response["ok"])
                self.assertIn("EVAL_AGENT_INPUT", response["error"])
                self.assertEqual(self.kernel.calls, [])
        self.launch_worker()
        for value in ("", "  ", 4, None):
            response = self.kernel.cell(f"await worker.send({value!r})")
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_AGENT_INPUT", response["error"])
        self.assertEqual(len(self.kernel.calls), 1)

    def test_worker_protocol_rejects_identity_or_turn_substitution_and_missing_results(self):
        self.launch_worker()
        invalid = [worker_snapshot("other"), worker_snapshot(turn_id=2),
                   {**worker_snapshot(), "jobId": "different"},
                   {**worker_snapshot(), "agent": "different"},
                   {**worker_snapshot(), "turn": {"id": 1, "status": "unknown", "result": None}},
                   worker_snapshot(status="completed"),
                   {**worker_snapshot(), "turnId": True}]
        for snapshot in invalid:
            response = self.kernel.cell("await worker.wait(0)", self.reply_with(envelope(snapshot)))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_AGENT_PROTOCOL", response["error"])
        response = self.kernel.cell("await worker.status()",
                                    lambda frame: self.kernel.reply(frame, error="PI_WORKER_OWNER_CHANGED"))
        self.assertFalse(response["ok"])
        self.assertIn("PI_WORKER_OWNER_CHANGED", response["error"])

    def create_pool(self, concurrency=2):
        response = self.kernel.cell(f"pool = await workpool(agent='scout', concurrency={concurrency}); pool.concurrency")
        self.assertEqual(self.value(response), concurrency)

    def test_pool_admits_batches_through_one_native_carrier_and_retains_fixed_items(self):
        self.create_pool()
        self.assertEqual(self.kernel.calls, [])
        native = PoolHost(self.kernel)
        response = self.kernel.cell(
            "items = await pool.push(['a','b','c','d','e']); [(item.pool_id, item.job_id, item.item_id) for item in items]", native)
        self.assertEqual(self.value(response), [("pool-native-1", "carrier-pool-native-1", item_id) for item_id in range(1, 6)])
        self.assertEqual(native.operations, ["pool_start"])
        self.assertEqual(self.kernel.calls[0]["bridge"]["input"]["args"], {
            "agent": "scout", "operation": "pool_start", "concurrency": 2, "tasks": ["a", "b", "c", "d", "e"],
        })
        self.assertEqual(self.value(self.kernel.cell("(hasattr(items[0], 'worker_id'), hasattr(items[0], 'cancel'))")), (False, False))
        response = self.kernel.cell("items[0].item_id = 7")
        self.assertFalse(response["ok"])
        native_pool = native.pools["pool-native-1"]
        native.complete(native_pool, 1)
        response = self.kernel.cell("more = await pool.push(['f']); more[0].item_id", native)
        self.assertEqual(self.value(response), 6)
        self.assertEqual(len(native.pools), 1)
        self.assertEqual(native.operations, ["pool_start", "pool_push"])
        before = len(self.kernel.calls)
        snapshot = self.value(self.kernel.cell("await pool.peek()", native))
        self.assertEqual(len(self.kernel.calls), before + 1)
        self.assertEqual(len(snapshot["items"]), 6)
        self.assertEqual(snapshot["items"][0]["summary"], "item 1")
        self.assertNotIn("result", snapshot["items"][0])
        self.assertEqual(self.value(self.kernel.cell("await items[0].output()", native))["details"]["turn"], 1)
        status = self.value(self.kernel.cell("await pool.status()", native))
        self.assertEqual(status["admittedItems"], 6)
        self.assertEqual(status["completedItems"], 1)
        fresh = Kernel()
        self.addCleanup(fresh.close)
        response = fresh.cell("await tool.result('subagent', {'operation':'pool_status', 'poolId':'pool-native-1'})",
                              lambda frame: fresh.reply(frame, json.dumps(envelope(native.apply(frame)))))
        self.assertEqual(self.value(response)["details"]["admittedItems"], 6)

    def test_pool_validates_whole_batch_and_enforces_total_capacity_before_dispatch(self):
        self.create_pool()
        for tasks in (["valid", ""], ["valid", None], ["valid", "x\0y"],
                      ["valid", "é" * 32769], ["valid", "\ud800"]):
            response = self.kernel.cell(f"await pool.push({tasks!r})")
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_POOL_INPUT", response["error"])
            self.assertEqual(self.kernel.calls, [])
        response = self.kernel.cell("await pool.push(['x' * 65536] * 4)")
        self.assertFalse(response["ok"])
        self.assertIn("encoded batch exceeds 256 KiB", response["error"])
        self.assertFalse(self.value(self.kernel.cell("pool.closed or pool.unknown_admission")))
        self.assertEqual(self.kernel.calls, [])
        native = PoolHost(self.kernel)
        response = self.kernel.cell("items = await pool.push([f'task {n}' for n in range(32)]); len(items)", native)
        self.assertEqual(self.value(response), 32)
        self.assertEqual(len(self.kernel.calls), 1)
        self.assertEqual(len(native.pools), 1)
        response = self.kernel.cell("await pool.push(['over capacity'])")
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_POOL_CAPACITY", response["error"])
        self.assertEqual(len(self.kernel.calls), 1)
        for options in ({"concurrency": 0}, {"concurrency": 9}, {"concurrency": 1.5}, {"concurrency": True},
                        {"timeout_seconds": 0}, {"scope": "all"}, {"isolation": "worktree"},
                        {"keep_alive": False}, {"output_schema": {}}, {"cwd": None}):
            response = self.kernel.cell(f"await workpool(**{options!r})")
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_POOL_INPUT", response["error"])
        self.assertEqual(len(self.kernel.calls), 1)

    def test_pool_launch_options_use_native_fields_and_keep_name_local(self):
        response = self.kernel.cell("pool = await workpool(agent='scout', concurrency=3, name='review', cwd='.', "
                                    "scope='project', isolation='none', timeout_seconds=120); pool.name")
        self.assertEqual(self.value(response), "review")
        native = PoolHost(self.kernel)
        self.value(self.kernel.cell("await pool.push(['inspect']); pool.id", native))
        self.assertEqual(self.kernel.calls[0]["bridge"]["input"]["args"], {
            "agent": "scout", "cwd": ".", "scope": "project", "isolation": "none", "timeoutSeconds": 120,
            "operation": "pool_start", "concurrency": 3, "tasks": ["inspect"],
        })

    def test_pool_lost_batch_acknowledgment_recovers_native_admission_without_replay(self):
        self.create_pool()
        native = PoolHost(self.kernel)
        self.value(self.kernel.cell("await pool.push(['a','b']); len(pool.items)", native))
        def bridge(frame):
            native.apply(frame)
            self.kernel.reply(frame, error="acknowledgment lost")
        response = self.kernel.cell("""
try:
    await pool.push(['c','d'])
except Exception as error:
    failure = (str(error), len(error.admitted), error.requested, len(pool.items), pool.closed, pool.unknown_admission)
failure
""", bridge)
        message, acknowledged, requested, retained, closed, unknown = self.value(response)
        self.assertIn("EVAL_POOL_ADMISSION", message)
        self.assertEqual((acknowledged, requested, retained, closed, unknown), (2, 2, 2, True, True))
        response = self.kernel.cell("await pool.push(['retry'])")
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_POOL_CLOSED", response["error"])
        snapshot = self.value(self.kernel.cell("await pool.peek()", native))
        self.assertEqual(len(snapshot["items"]), 4)
        self.assertTrue(snapshot["closed"])
        self.assertFalse(snapshot["unknownAdmission"])
        self.assertEqual(self.value(self.kernel.cell("pool.items[-1].item_id")), 4)
        self.assertEqual(native.operations, ["pool_start", "pool_push", "pool_status"])
        closed = self.value(self.kernel.cell("await pool.close()", native))
        self.assertTrue(closed["allClosed"])

    def test_pool_finish_seals_once_and_returns_a_retained_aggregate_job(self):
        self.create_pool(concurrency=1)
        native = PoolHost(self.kernel)
        response = self.kernel.cell("""
import asyncio
settled = await asyncio.gather(pool.push(['a','b']), pool.finish(), pool.push(['late']), return_exceptions=True)
(len(settled[0]), settled[1].id, str(settled[2]))
""", native)
        count, job_id, rejected = self.value(response)
        self.assertEqual((count, job_id), (2, "carrier-pool-native-1"))
        self.assertIn("EVAL_POOL_CLOSED", rejected)
        self.assertEqual(native.operations, ["pool_start", "pool_finish"])
        self.assertEqual(self.value(self.kernel.cell("(await pool.finish()).id")), job_id)
        result = self.value(self.kernel.cell("await pool.wait(10)", native))
        self.assertEqual(result["details"]["status"], "completed")
        self.assertEqual([item["id"] for item in result["details"]["items"]], [1, 2])
        self.assertEqual(native.operations, ["pool_start", "pool_finish", "jobs:wait"])
        self.assertEqual(self.value(self.kernel.cell("await pool.output()", native))["details"]["completedItems"], 2)
        response = self.kernel.cell("await pool.wait(-1)")
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_AGENT_INPUT", response["error"])
        self.create_pool()
        self.assertEqual(self.value(self.kernel.cell("(await pool.finish()).id", native)), "carrier-pool-native-2")
        self.assertEqual(native.operations[-2:], ["pool_start", "pool_finish"])
        self.assertEqual(self.kernel.calls[-2]["bridge"]["input"]["args"]["tasks"], [])

    def test_pool_items_participate_in_ordered_mixed_waits_with_one_deadline(self):
        self.launch_agent("job", "job-once")
        self.launch_worker()
        self.create_pool()
        native = PoolHost(self.kernel)
        self.value(self.kernel.cell("items = await pool.push(['a']); items[0].item_id", native))
        native.complete(native.pools["pool-native-1"], 1, {**turn_result(1), "isError": True})
        requests = []
        def bridge(frame):
            request = frame["bridge"]["input"]
            requests.append(request)
            self.assertGreaterEqual(request["args"]["timeoutMs"], 0)
            self.assertLessEqual(request["args"]["timeoutMs"], 10000)
            if request["args"].get("operation") == "pool_wait":
                native(frame)
            elif request["name"] == "jobs":
                self.kernel.reply(frame, json.dumps(envelope({"schema": "pi.host_job.v1", "id": "job-once",
                    "status": "exited", "result": turn_result(0)})))
            else:
                self.kernel.reply(frame, json.dumps(envelope(worker_snapshot(status="completed", result=turn_result(2)))))
        results = self.value(self.kernel.cell("await wait([items[0], job, worker], timeout=10, raise_errors=False)", bridge))
        self.assertEqual([result["details"]["turn"] for result in results], [1, 0, 2])
        self.assertTrue(results[0]["isError"])
        self.assertEqual([request["args"].get("operation", request["args"].get("action"))
                          for request in requests], ["pool_wait", "wait", "worker_wait"])

    def test_pool_item_timeout_and_protocol_validation_preserve_native_identity(self):
        self.create_pool()
        native = PoolHost(self.kernel)
        self.value(self.kernel.cell("items = await pool.push(['a']); items[0].id", native))
        response = self.kernel.cell("await items[0].wait(0)", native)
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_AGENT_TIMEOUT", response["error"])
        self.assertEqual(native.operations, ["pool_start", "pool_wait"])
        pool = native.pools["pool-native-1"]
        native.complete(pool, 1)
        good = {**native.snapshot(pool), "itemId": 1, "item": {"id": 1, "status": "completed", "result": turn_result(1)}}
        invalid = [{**good, "poolId": "other"}, {**good, "jobId": "other"}, {**good, "agent": "other"},
                   {**good, "itemId": 2}, {**good, "itemId": True}, {**good, "completedItems": 0},
                   {**good, "item": {"id": 1, "status": "completed", "result": None}},
                   {**good, "items": [{**good["items"][0], "resultRef": {"poolId": pool["id"], "itemId": 2}}]}]
        for snapshot in invalid:
            response = self.kernel.cell("await items[0].status()", self.reply_with(envelope(snapshot)))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_POOL_PROTOCOL", response["error"])
        response = self.kernel.cell("await items[0].output()",
                                    lambda frame: self.kernel.reply(frame, error="PI_POOL_OWNER_CHANGED"))
        self.assertFalse(response["ok"])
        self.assertIn("PI_POOL_OWNER_CHANGED", response["error"])

    def test_pool_serializes_concurrent_push_close_and_late_admission(self):
        self.create_pool(concurrency=1)
        native = PoolHost(self.kernel)
        response = self.kernel.cell("""
import asyncio
pushing = asyncio.create_task(pool.push(['a', 'b']))
closing = asyncio.create_task(pool.close())
late = asyncio.create_task(pool.push(['too late']))
settled = await asyncio.gather(pushing, closing, late, return_exceptions=True)
(len(settled[0]), settled[1]['allClosed'], str(settled[2]))
""", native)
        count, closed, message = self.value(response)
        self.assertEqual(count, 2)
        self.assertTrue(closed)
        self.assertIn("EVAL_POOL_CLOSED", message)
        self.assertEqual(native.operations, ["pool_start", "pool_close"])
        self.assertTrue(self.value(self.kernel.cell("await pool.close()"))["allClosed"])
        self.assertEqual(len(self.kernel.calls), 2)
        self.create_pool()
        self.assertTrue(self.value(self.kernel.cell("await pool.close()"))["allClosed"])
        self.assertEqual(len(self.kernel.calls), 2)

    def test_cancelled_pool_admission_releases_lock_and_keeps_known_carrier_closable(self):
        self.create_pool(concurrency=1)
        native = PoolHost(self.kernel)
        self.value(self.kernel.cell("await pool.push(['a']); len(pool.items)", native))
        pending = []
        def bridge(frame):
            if frame["bridge"]["input"]["args"]["operation"] == "pool_push":
                pending.append(frame)
            else:
                native(frame)
        def observe(frame):
            if "release-pool-reply" in frame["stream"].get("text", ""):
                self.assertEqual(len(pending), 1)
                native(pending[0])
        response = self.kernel.cell("""
import asyncio
running = asyncio.create_task(pool.push(['b', 'c']))
await asyncio.sleep(0.03)
running.cancel()
print('release-pool-reply')
try:
    await running
except asyncio.CancelledError:
    pass
(len(pool.items), pool.closed)
""", bridge, observe)
        self.assertEqual(self.value(response), (1, True))
        closed = self.value(self.kernel.cell("await pool.close()", native))
        self.assertTrue(closed["allClosed"])
        self.assertEqual(len(closed["items"]), 3)
        self.assertEqual(native.operations, ["pool_start", "pool_push", "pool_close"])

    def test_pool_lost_start_acknowledgment_never_claims_the_unknown_carrier_was_closed(self):
        self.create_pool()
        response = self.kernel.cell("await pool.push(['first'])",
                                    lambda frame: self.kernel.reply(frame, error="acknowledgment lost"))
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_POOL_ADMISSION", response["error"])
        closed = self.value(self.kernel.cell("await pool.close()"))
        self.assertTrue(closed["allKnownClosed"])
        self.assertTrue(closed["unknownAdmission"])
        self.assertFalse(closed["allClosed"])
        response = self.kernel.cell("await pool.finish()")
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_POOL_CLOSED", response["error"])
        self.assertEqual(len(self.kernel.calls), 1)

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
