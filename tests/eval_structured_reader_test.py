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


def model_ack(operation, job_id="model-native", timeout_seconds=None):
    return {"schema": "pi.eval.model-job.v1", "operation": operation, "jobId": job_id,
            "timeoutSeconds": timeout_seconds if timeout_seconds is not None
            else 300 if operation == "judgment_batch" else 60}


def judgment_value(value):
    return {"schema": "pi.judgment.v1", "answers": {"safe": {"type": "bool", "bool": value}}}


def model_result(operation, result, *, failed=False, error=None):
    return {"content": [{"type": "text", "text": "model request failed" if failed else "model request completed"}],
            "details": {"schema": "pi.eval.model-job.result.v1", "operation": operation,
                        "result": result, "error": error,
                        "judgments": {"schema": "pi.eval.judgments.v1", "attempts": [
                            {"attempt": 1, "status": "failed" if failed else "completed"}]}},
            **({"isError": True} if failed else {})}


def model_snapshot(operation, result=None, *, job_id="model-native", status="exited"):
    return {"schema": "pi.host_job.v1", "id": job_id,
            "command": "eval model completion" if operation == "completion" else f"eval {operation}", "status": status,
            **({"result": result} if status != "running" else {})}


def batch_progress_result(interrupted=False):
    items = [{"key": "ready", "status": "completed", "verdict": judgment_value(1)},
             {"key": "bad", "status": "failed", "error": "PI_JUDGE_PROVIDER"},
             {"key": "active", "status": "running"}, {"key": "later", "status": "queued"}]
    partial = {"content": [{"type": "text", "text": "2 of 4 judgments completed"}], "details": {
        "schema": "pi.eval.model-job.progress.v1", "operation": "judgment_batch", "complete": False,
        "total": 4, "completed": 2, "succeeded": 1, "failed": 1, "queued": 1, "running": 1, "items": items,
        "judgments": {"schema": "pi.eval.judgments.v1", "attempts": [
            {"attempt": index + 1, "operation": "judgment", "status": item["status"], "usage": None, "usageKnown": False}
            for index, item in enumerate(items)]}}}
    return {"content": [{"type": "text", "text": "job interrupted" if interrupted else "job running"}],
            "details": {"schema": "pi.host_job.interrupted.v1" if interrupted else "pi.host_job.progress.v1",
                        "partialResult": partial}, **({"isError": True} if interrupted else {})}


def batch_progress_snapshot(native, status="running", job_id="model-native"):
    return {**model_snapshot("judgment_batch", native, status=status, job_id=job_id), "result": native}


def batch_drain_cursor(seen=(), manifest="a" * 64):
    return {"schema": "pi.eval.batch-cursor.v1", "jobId": "model-native", "manifest": manifest, "seen": list(seen)}


def batch_drain_item(index, key=None, *, error=None):
    item = {"index": index, "key": index if key is None else key, "ok": error is None}
    if error is None:
        item["verdict"] = {"schema": "pi.judgment.v1", "answers": {
            "safe": {"type": "bool", "bool": False}, "score": {"type": "number", "number": 0}},
            "provider": "fixture", "modelId": "reviewer", "usage": {"input": 9, "output": 3}, "extra": None}
    else:
        item["error"] = error
    return item


def batch_drain_page(**updates):
    return {"schema": "pi.eval.batch-drain.v1", "jobId": "model-native", "jobStatus": "running",
            "done": False, "jobFailed": False, "complete": False, "evidenceAvailable": True,
            "total": 2, "settled": 1, "items": [batch_drain_item(1)], "cursor": batch_drain_cursor([1]), **updates}


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

    def launch_model(self, expression="completion('work')", operation="completion",
                     job_id="model-native", variable="model"):
        def bridge(frame):
            self.assertEqual(frame["bridge"]["tool"], {
                "completion": "__pi_model_complete_start", "judgment": "__pi_judge_start",
                "judgment_batch": "__pi_judge_batch_start",
            }[operation])
            self.kernel.reply(frame, json.dumps(model_ack(operation, job_id,
                frame["bridge"]["input"].get("timeout_secs"))))
        response = self.kernel.cell(f"{variable} = await {expression}; {variable}.id", bridge)
        self.assertEqual(self.value(response), job_id)
        return self.kernel.calls[-1]["bridge"]["input"]

    def test_model_factories_preserve_options_and_image_evidence_in_native_requests(self):
        request = self.launch_model("completion('summarize', model='smol', system='be concise', schema={'type':'object'}, "
                                    "images=['chart.png'], max_tokens=256, timeout_seconds=17)")
        self.assertEqual(request, {"prompt": "summarize", "model": "smol", "system": "be concise", "schema": {"type": "object"},
                                   "images": ["chart.png"], "max_tokens": 256, "timeout_secs": 17})
        native = model_result("completion", {"schema": "pi.completion.v1", "text": '{"answer":42}',
                                              "data": {"answer": 42}, "schemaValid": True})
        response = self.kernel.cell("await model.wait(5)", self.reply_with(envelope(model_snapshot("completion", native))))
        self.assertEqual(self.value(response), {"answer": 42})
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
            "name": "jobs", "args": {"action": "wait", "jobId": "model-native", "timeoutMs": 5000},
        })
        request = self.launch_model("judge({'view':'screen'}, {'safe':{'type':'bool','instructions':'safe?'}}, "
                                    "model='slow', images=['shot.png'], timeout_seconds=8)", "judgment", "judge-native", "review")
        self.assertEqual(request["model"], "slow")
        self.assertEqual(request["images"], ["shot.png"])
        self.assertEqual(request["timeout_secs"], 8)
        request = self.launch_model("judge_batch([{'id':1},{'id':2}], {'safe':{'type':'bool','instructions':'safe?'}}, "
                                    "model='tiny', images={'1':['b.png']}, concurrency=2)", "judgment_batch", "batch-native", "batch")
        self.assertEqual(request["model"], "tiny")
        self.assertEqual(request["images"], {"1": ["b.png"]})
        self.assertEqual(request["concurrency"], 2)
        self.assertEqual(len(self.kernel.calls), 4)

    def test_model_results_preserve_false_zero_null_and_full_native_receipts(self):
        for data in (None, False, 0):
            with self.subTest(data=data):
                self.launch_model("completion('extract', schema=True)")
                native = model_result("completion", {"schema": "pi.completion.v1", "text": json.dumps(data),
                                                      "data": data, "schemaValid": True})
                response = self.kernel.cell("await model", self.reply_with(envelope(model_snapshot("completion", native))))
                if data is None:
                    self.assertTrue(response["ok"], response.get("error"))
                    self.assertIsNone(response["result"])
                else:
                    self.assertEqual(self.value(response), data)
                response = self.kernel.cell("await model.output()", self.reply_with(envelope(model_snapshot("completion", native))))
                self.assertEqual(self.value(response), native)
        self.launch_model("judge('evidence', {'safe':{'type':'bool','instructions':'safe?'}})", "judgment")
        response = self.kernel.cell("await model.wait()", self.reply_with(envelope(
            model_snapshot("judgment", model_result("judgment", judgment_value(0))))))
        self.assertEqual(self.value(response)["safe"]["bool"], 0)

    def test_model_mixed_batch_failures_preserve_ordered_answers_and_receipts(self):
        self.launch_model("judge_batch(['good','bad'], {'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")
        payload = {"schema": "pi.eval.judgment-batch.v1", "batch": 1, "total": 2, "succeeded": 1, "failed": 1,
                   "allOk": False, "items": [{"key": 0, "ok": True, "verdict": judgment_value(1)},
                                             {"key": 1, "ok": False, "error": "PI_JUDGE_PROVIDER"}]}
        native = model_result("judgment_batch", payload, failed=True)
        snapshot = model_snapshot("judgment_batch", native, status="failed")
        response = self.kernel.cell("await model.wait(0, raise_errors=False)", self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response), payload)
        response = self.kernel.cell("""
try:
    await model.wait(0)
except Exception as error:
    captured = (error.job_id, error.result)
captured
""", self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response), ("model-native", native))
        response = self.kernel.cell("await model.output()", self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response), native)

    def launch_batch(self):
        self.launch_model("judge_batch([1,2], {'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")

    def test_batch_drain_uses_one_native_waiter_call_and_preserves_full_page(self):
        self.launch_batch()
        page = batch_drain_page()
        response = self.kernel.cell("await model.drain(timeout=0.025)", self.reply_with(page))
        self.assertEqual(self.value(response), page)
        self.assertEqual(self.kernel.calls[-1]["bridge"]["tool"], "__pi_judge_batch_drain")
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
            "jobId": "model-native", "cursor": None, "timeoutMs": 25})
        self.assertEqual(len(self.kernel.calls), 2)

    def test_batch_drain_acknowledgment_is_explicit_and_pages_can_finish_out_of_order(self):
        self.launch_batch()
        first = batch_drain_page()
        response = self.kernel.cell("""
bookmark = None
page = await model.drain(cursor=bookmark)
raise RuntimeError('processing failed')
""", self.reply_with(first))
        self.assertFalse(response["ok"])
        self.assertIn("processing failed", response["error"])
        response = self.kernel.cell("page = await model.drain(cursor=bookmark); page", self.reply_with(first))
        self.assertEqual(self.value(response), first)
        for frame in self.kernel.calls[1:]:
            self.assertEqual(frame["bridge"]["input"], {"jobId": "model-native", "cursor": None, "timeoutMs": 30000})
        self.assertEqual(self.value(self.kernel.cell("bookmark = page['cursor']; bookmark")), batch_drain_cursor([1]))
        second = batch_drain_page(jobStatus="exited", done=True, complete=True, settled=2,
                                 items=[batch_drain_item(0)], cursor=batch_drain_cursor([0, 1]))
        response = self.kernel.cell("page = await model.drain(cursor=bookmark, timeout=0); page", self.reply_with(second))
        self.assertEqual(self.value(response), second)
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"]["cursor"], batch_drain_cursor([1]))
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"]["timeoutMs"], 0)
        response = self.kernel.cell("await model.drain(cursor=page['cursor'], timeout=0)", self.reply_with({**second, "items": []}))
        self.assertEqual(self.value(response)["items"], [])

    def test_batch_drain_preserves_literal_keys_and_explicit_item_failures(self):
        self.launch_batch()
        page = batch_drain_page(jobStatus="failed", done=True, jobFailed=True, complete=True, settled=2,
                               items=[batch_drain_item(0, "雪"), batch_drain_item(1, "__proto__", error="PI_JUDGE_PROVIDER")],
                               cursor=batch_drain_cursor([0, 1]))
        response = self.kernel.cell("await model.drain()", self.reply_with(page))
        self.assertEqual(self.value(response), page)

    def test_batch_drain_interruption_and_withdrawal_preserve_honest_partial_evidence(self):
        self.launch_batch()
        for status in ("killed", "timedOut"):
            page = batch_drain_page(jobStatus=status, done=True, jobFailed=True)
            response = self.kernel.cell("await model.drain()", self.reply_with(page))
            self.assertEqual(self.value(response), page)
            self.assertFalse(self.value(response)["complete"])
        empty = batch_drain_page(evidenceAvailable=False, total=None, settled=None, items=[],
                                cursor=batch_drain_cursor([], None))
        response = self.kernel.cell("await model.drain(timeout=0)", self.reply_with(empty))
        self.assertEqual(self.value(response), empty)
        response = self.kernel.cell("bookmark = (await model.drain())['cursor']; bookmark", self.reply_with(batch_drain_page()))
        self.assertEqual(self.value(response), batch_drain_cursor([1]))
        withdrawn = {**empty, "jobStatus": "failed", "done": True, "jobFailed": True, "cursor": batch_drain_cursor([1])}
        response = self.kernel.cell("await model.drain(cursor=bookmark)", self.reply_with(withdrawn))
        self.assertEqual(self.value(response), withdrawn)
        self.assertEqual(self.value(self.kernel.cell("bookmark")), batch_drain_cursor([1]))

    def test_batch_drain_cursor_can_be_reused_after_fresh_kernel_attachment(self):
        self.launch_batch()
        saved = self.value(self.kernel.cell("(await model.drain())['cursor']", self.reply_with(batch_drain_page())))
        self.kernel = Kernel()
        self.addCleanup(self.kernel.close)
        response = self.kernel.cell("model = await judge_batch.attach('model-native'); model.id",
                                    self.reply_with(envelope(model_snapshot("judgment_batch", status="running"))))
        self.assertEqual(self.value(response), "model-native")
        page = batch_drain_page(jobStatus="exited", done=True, complete=True, settled=2,
                               items=[batch_drain_item(0)], cursor=batch_drain_cursor([0, 1]))
        response = self.kernel.cell(f"await model.drain(cursor={saved!r}, timeout=0)", self.reply_with(page))
        self.assertEqual(self.value(response), page)
        self.assertEqual([frame["bridge"]["tool"] for frame in self.kernel.calls], ["__pi_tool_result", "__pi_judge_batch_drain"])

    def test_batch_drain_native_failures_do_not_acknowledge_retry_or_poison_the_bridge(self):
        self.launch_batch()
        self.assertEqual(self.value(self.kernel.cell(f"bookmark = {batch_drain_cursor([1])!r}; bookmark")), batch_drain_cursor([1]))
        for message in ("EVAL_BATCH_TIMEOUT", "EVAL_BATCH_HOST: current jobs lookup was refused", "PI_JOBS_UNKNOWN_ID"):
            before = len(self.kernel.calls)
            response = self.kernel.cell("await model.drain(cursor=bookmark)",
                                        lambda frame: self.kernel.reply(frame, error=message))
            self.assertFalse(response["ok"])
            self.assertIn(message, response["error"])
            self.assertNotIn("EVAL_BATCH_PROTOCOL", response["error"])
            self.assertEqual(len(self.kernel.calls), before + 1)
            self.assertEqual(self.value(self.kernel.cell("bookmark")), batch_drain_cursor([1]))
        self.assert_next_cell_works()

    def test_batch_drain_rejects_nonbatch_handles_without_dispatch(self):
        for operation, expression in (("completion", "completion('work')"),
                                      ("judgment", "judge('work', {'safe':{'type':'bool','instructions':'safe?'}})")):
            self.launch_model(expression, operation)
            before = len(self.kernel.calls)
            response = self.kernel.cell("await model.drain()")
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_BATCH_INPUT", response["error"])
            self.assertEqual(len(self.kernel.calls), before)

    def test_batch_drain_validates_timeout_and_cursor_before_dispatch(self):
        self.launch_batch()
        for timeout in ("True", "None", "'30'", "-1", "601", "float('inf')", "float('nan')", "10 ** 1000"):
            response = self.kernel.cell(f"await model.drain(timeout={timeout})")
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_BATCH_INPUT", response["error"])
        invalid = [None, [], "cursor", {}, batch_drain_cursor([1, 1]), batch_drain_cursor([-1]),
                   batch_drain_cursor([16]), batch_drain_cursor([True]), batch_drain_cursor([1.0]),
                   batch_drain_cursor([0], None), batch_drain_cursor([], "A" * 64), batch_drain_cursor([], "a" * 63),
                   {**batch_drain_cursor(), "jobId": "foreign"}, {**batch_drain_cursor(), "schema": "wrong"},
                   {**batch_drain_cursor(), "extra": 1}, {**batch_drain_cursor(), "seen": (1,)}]
        for cursor in invalid[1:]:
            with self.subTest(cursor=cursor):
                response = self.kernel.cell(f"await model.drain(cursor={cursor!r})")
                self.assertFalse(response["ok"])
                self.assertIn("EVAL_BATCH_INPUT", response["error"])
        for expression in ("model.drain(retries=1)", "model.drain(None, 30)"):
            response = self.kernel.cell(f"await {expression}")
            self.assertFalse(response["ok"])
            self.assertIn("TypeError", response["error"])
        self.assertEqual(len(self.kernel.calls), 1)

    def test_batch_drain_does_not_execute_cursor_subclass_callbacks(self):
        self.launch_batch()
        response = self.kernel.cell(f"""
class Trap(dict):
    def __iter__(self):
        raise AssertionError('cursor callback ran')
class SeenTrap(list):
    def __iter__(self):
        raise AssertionError('seen callback ran')
errors = []
for bookmark in (Trap({batch_drain_cursor()!r}), {{**{batch_drain_cursor()!r}, 'seen': SeenTrap([1])}}):
    try:
        await model.drain(cursor=bookmark)
    except TypeError as error:
        errors.append(str(error))
errors
""")
        self.assertTrue(all("EVAL_BATCH_INPUT" in error for error in self.value(response)))
        self.assertEqual(len(self.value(response)), 2)
        self.assertEqual(len(self.kernel.calls), 1)

    def test_batch_drain_snapshots_cursor_before_awaiting_the_host(self):
        self.launch_batch()
        response = self.kernel.cell(f"""
import asyncio
bookmark = {batch_drain_cursor([1])!r}
pending = asyncio.create_task(model.drain(cursor=bookmark))
await asyncio.sleep(0)
bookmark['seen'].append(0)
bookmark['manifest'] = 'changed'
await pending
""", self.reply_with(batch_drain_page(items=[])))
        self.assertEqual(self.value(response), batch_drain_page(items=[]))
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"]["cursor"], batch_drain_cursor([1]))

    def test_batch_drain_cancellation_drains_reply_without_acknowledging_or_cancelling_job(self):
        self.launch_batch()
        pending = []

        def observe(frame):
            if "release-drain" in frame["stream"].get("text", ""):
                self.assertEqual(len(pending), 1)
                self.kernel.reply(pending[0], json.dumps(batch_drain_page()))

        response = self.kernel.cell("""
import asyncio
bookmark = None
pending = asyncio.create_task(model.drain(cursor=bookmark))
await asyncio.sleep(0.02)
pending.cancel()
print('release-drain')
try:
    page = await pending
    bookmark = page['cursor']
    outcome = 'unexpected success'
except asyncio.CancelledError:
    outcome = 'cancelled'
(outcome, bookmark)
""", pending.append, observe)
        self.assertEqual(self.value(response), ("cancelled", None))
        response = self.kernel.cell("await model.drain(cursor=bookmark, timeout=0)", self.reply_with(batch_drain_page()))
        self.assertEqual(self.value(response), batch_drain_page())
        self.assertEqual([frame["bridge"]["tool"] for frame in self.kernel.calls],
                         ["__pi_judge_batch_start", "__pi_judge_batch_drain", "__pi_judge_batch_drain"])

    def test_batch_drain_rejects_malformed_pages_and_keeps_errors_distinct(self):
        self.launch_batch()
        corruptions = [(("schema",), "wrong"), (("jobId",), "foreign"), (("jobStatus",), "unknown"),
            (("done",), True), (("done",), 0), (("jobFailed",), True), (("complete",), True),
            (("complete",), 0), (("evidenceAvailable",), 1), (("total",), 0), (("total",), 17),
            (("total",), True), (("settled",), 2), (("settled",), True), (("cursor",), None),
            (("cursor", "seen"), [0, 1]), (("items", 0, "index"), True), (("items", 0, "key"), 0),
            (("items", 0, "key"), False), (("items", 0, "ok"), 1), (("items", 0, "verdict"), None),
            (("items", 0, "error"), "also failed"), (("evidenceAvailable",), False)]
        for path, value in corruptions:
            with self.subTest(path=path, value=value):
                page = batch_drain_page()
                target = page
                for component in path[:-1]:
                    target = target[component]
                target[path[-1]] = value
                response = self.kernel.cell("await model.drain()", self.reply_with(page))
                self.assertFalse(response["ok"])
                self.assertIn("EVAL_BATCH_PROTOCOL", response["error"])
        for items in ([batch_drain_item(1), batch_drain_item(0)],
                      [batch_drain_item(0, "same"), batch_drain_item(1, "same")],
                      [batch_drain_item(0), batch_drain_item(1, "literal")],
                      [{"index": 0, "key": 0, "ok": False, "error": ""}, batch_drain_item(1)]):
            page = batch_drain_page(settled=2, items=items, cursor=batch_drain_cursor([0, 1]))
            response = self.kernel.cell("await model.drain()", self.reply_with(page))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_BATCH_PROTOCOL", response["error"])
        for encoded in ("not JSON", "null", "[]", "{}"):
            response = self.kernel.cell("await model.drain()", lambda frame: self.kernel.reply(frame, encoded))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_BATCH_PROTOCOL", response["error"])
        self.assert_next_cell_works()

    def test_batch_drain_cannot_change_manifest_redeliver_or_forget_acknowledged_items(self):
        self.launch_batch()
        for page in (batch_drain_page(), batch_drain_page(items=[], cursor=batch_drain_cursor([1], "b" * 64)),
                     batch_drain_page(settled=0, items=[], cursor=batch_drain_cursor())):
            response = self.kernel.cell(f"await model.drain(cursor={batch_drain_cursor([1])!r})", self.reply_with(page))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_BATCH_PROTOCOL", response["error"])
        for missing in ("total", "settled"):
            page = batch_drain_page(evidenceAvailable=False, total=None, settled=None, items=[],
                                   cursor=batch_drain_cursor([], None))
            page.pop(missing)
            response = self.kernel.cell("await model.drain(timeout=0)", self.reply_with(page))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_BATCH_PROTOCOL", response["error"])

    def test_batch_progress_exposes_ordered_evidence_and_each_observation_reenters_host(self):
        self.launch_model("judge_batch({'ready':1,'bad':2,'active':3,'later':4}, "
                          "{'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")
        native = batch_progress_result()
        snapshot = batch_progress_snapshot(native)
        progress = {**native["details"]["partialResult"]["details"], "jobStatus": "running"}
        for method, expected in (("progress", progress), ("status", snapshot), ("output", native), ("done", False)):
            response = self.kernel.cell(f"await model.{method}()", self.reply_with(envelope(snapshot)))
            self.assertEqual(self.value(response), expected)
            self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
                "name": "jobs", "args": {"action": "wait", "jobId": "model-native", "timeoutMs": 0}})
        self.assertEqual(len(self.kernel.calls), 5)

    def test_partial_batch_results_never_satisfy_individual_or_global_waits(self):
        self.launch_model("judge_batch([1,2,3,4], {'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")
        for expression in ("model.wait(0)", "model.wait(0, raise_errors=False)",
                           "wait([model], timeout=0, raise_errors=False)"):
            response = self.kernel.cell(f"await {expression}", self.reply_with(envelope(
                batch_progress_snapshot(batch_progress_result()))))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_MODEL_TIMEOUT", response["error"])
        self.assertEqual(sum(frame["bridge"]["tool"] == "__pi_judge_batch_start" for frame in self.kernel.calls), 1)
        self.assertTrue(all(frame["bridge"]["input"]["args"]["action"] == "wait" for frame in self.kernel.calls[1:]))

    def test_interrupted_batches_retain_evidence_without_claiming_remaining_work_continues(self):
        for status in ("killed", "timedOut"):
            with self.subTest(status=status):
                self.launch_model("judge_batch([1,2,3,4], {'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")
                native = batch_progress_result(interrupted=True)
                native["details"]["result"] = "unrelated wrapper metadata"
                snapshot = batch_progress_snapshot(native, status)
                progress = {**native["details"]["partialResult"]["details"], "jobStatus": status}
                for expression, expected in (("model.progress()", progress), ("model.done()", True),
                        ("model.output()", native), ("model.wait(0, raise_errors=False)", native),
                        ("wait([model], timeout=0, raise_errors=False)", [native])):
                    response = self.kernel.cell(f"await {expression}", self.reply_with(envelope(snapshot)))
                    self.assertEqual(self.value(response), expected)
                response = self.kernel.cell("""
try:
    await model.wait(0)
except Exception as error:
    captured = (error.job_id, error.result)
captured
""", self.reply_with(envelope(snapshot)))
                self.assertEqual(self.value(response), ("model-native", native))
                if status == "killed":
                    response = self.kernel.cell("await model.cancel()", self.reply_with(envelope(snapshot)))
                    self.assertEqual(self.value(response)["status"], "killed")
                    self.assertEqual(self.kernel.calls[-1]["bridge"]["input"]["args"]["action"], "cancel")

    def test_settled_batch_progress_derives_complete_counts_from_final_aggregate(self):
        for failed in (False, True):
            with self.subTest(failed=failed):
                self.launch_model("judge_batch([1,2], {'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")
                items = [{"key": "first", "ok": True, "verdict": judgment_value(1)},
                         {"key": "second", "ok": False, "error": "PI_JUDGE_PROVIDER"} if failed
                         else {"key": "second", "ok": True, "verdict": judgment_value(0)}]
                payload = {"schema": "pi.eval.judgment-batch.v1", "total": 2, "succeeded": 1 if failed else 2,
                           "failed": 1 if failed else 0, "allOk": not failed, "items": items}
                native = model_result("judgment_batch", payload, failed=failed)
                native["details"]["judgments"]["attempts"] = [
                    {"attempt": index + 1, "status": "completed" if item["ok"] else "failed"}
                    for index, item in enumerate(items)]
                status = "failed" if failed else "exited"
                response = self.kernel.cell("await model.progress()", self.reply_with(envelope(
                    model_snapshot("judgment_batch", native, status=status))))
                self.assertEqual(self.value(response), {
                    "schema": "pi.eval.model-job.progress.v1", "operation": "judgment_batch", "complete": True,
                    "jobStatus": status, "total": 2, "completed": 2, "succeeded": payload["succeeded"],
                    "failed": payload["failed"], "queued": 0, "running": 0,
                    "items": [{"key": "first", "status": "completed", "verdict": judgment_value(1)},
                              {"key": "second", "status": "failed", "error": "PI_JUDGE_PROVIDER"} if failed
                              else {"key": "second", "status": "completed", "verdict": judgment_value(0)}],
                    "judgments": native["details"]["judgments"]})

    def test_batch_progress_is_none_before_first_update_and_for_unpreserved_failures(self):
        self.launch_model("judge_batch([1,2], {'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")
        failure = {"content": [], "details": None, "isError": True}
        for snapshot in (model_snapshot("judgment_batch", status="running"), batch_progress_snapshot(None),
                         batch_progress_snapshot(failure, "failed"), batch_progress_snapshot(failure, "killed"),
                         batch_progress_snapshot(model_result("judgment_batch", None, failed=True,
                                                 error="PI_JUDGE_PRIVACY"), "failed")):
            response = self.kernel.cell("await model.progress()", self.reply_with(envelope(snapshot)))
            self.assertTrue(response["ok"], response.get("error"))
            self.assertIsNone(response["result"])

    def test_batch_attachment_recovers_partial_results_without_caching_old_access(self):
        snapshot = batch_progress_snapshot(batch_progress_result(), job_id="recovered")
        response = self.kernel.cell("model = await judge_batch.attach('recovered'); model.id",
                                    self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response), "recovered")
        response = self.kernel.cell("await model.progress()", self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response)["completed"], 2)
        response = self.kernel.cell("await model.progress()", lambda frame: self.kernel.reply(frame, error="PI_JOBS_UNKNOWN_ID"))
        self.assertFalse(response["ok"])
        self.assertIn("PI_JOBS_UNKNOWN_ID", response["error"])
        self.assertEqual(len(self.kernel.calls), 3)
        self.assertTrue(all(frame["bridge"]["tool"] == "__pi_tool_result" for frame in self.kernel.calls))

    def test_batch_progress_validates_counts_unique_keys_evidence_and_receipts(self):
        self.launch_model("judge_batch([1,2,3,4], {'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")
        prefix = ("details", "partialResult", "details")
        corruptions = [(("details", "schema"), "wrong"), (("isError",), True),
            (("details", "partialResult", "isError"), True), (("details", "partialResult", "content"), None),
            (prefix + ("schema",), "wrong"), (prefix + ("operation",), "completion"),
            (prefix + ("complete",), True), (prefix + ("total",), 5), (prefix + ("completed",), 3),
            (prefix + ("succeeded",), True), (prefix + ("running",), -1),
            (prefix + ("items", 3, "key"), "ready"), (prefix + ("items", 3, "key"), ""),
            (prefix + ("items", 3, "key"), False), (prefix + ("items", 0, "status"), ["completed"]),
            (prefix + ("items", 0, "verdict"), None), (prefix + ("items", 0, "error"), "contradiction"),
            (prefix + ("items", 1, "error"), ""), (prefix + ("items", 1, "verdict"), judgment_value(1)),
            (prefix + ("items", 2, "verdict"), judgment_value(1)), (prefix + ("items", 3, "error"), None),
            (prefix + ("items", 3, "status"), "running"), (prefix + ("judgments", "schema"), "wrong"),
            (prefix + ("judgments", "attempts"), []), (prefix + ("judgments", "attempts", 0), None)]
        for path, value in corruptions:
            with self.subTest(path=path, value=value):
                native = batch_progress_result()
                target = native
                for component in path[:-1]:
                    target = target[component]
                target[path[-1]] = value
                response = self.kernel.cell("await model.status()", self.reply_with(envelope(batch_progress_snapshot(native))))
                self.assertFalse(response["ok"])
                self.assertIn("EVAL_MODEL_PROTOCOL", response["error"])
        native = batch_progress_result()
        items = native["details"]["partialResult"]["details"]["items"]
        items[0]["key"], items[1]["key"] = 0, "0"
        response = self.kernel.cell("await model.progress()", self.reply_with(envelope(batch_progress_snapshot(native))))
        self.assertEqual([item["key"] for item in self.value(response)["items"]], [0, "0", "active", "later"])

    def test_progress_wrappers_only_fit_batches_and_matching_running_or_interrupted_states(self):
        self.launch_model("judge_batch([1,2,3,4], {'safe':{'type':'bool','instructions':'safe?'}})", "judgment_batch")
        for native, status in ((batch_progress_result(True), "running"), (batch_progress_result(True), "exited"),
                               (batch_progress_result(True), "failed"), (batch_progress_result(), "killed"),
                               (batch_progress_result(), "timedOut")):
            response = self.kernel.cell("await model.status()", self.reply_with(envelope(batch_progress_snapshot(native, status))))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_MODEL_PROTOCOL", response["error"])
        for operation, expression in (("completion", "completion('work')"),
                                      ("judgment", "judge('work', {'safe':{'type':'bool','instructions':'safe?'}})")):
            self.launch_model(expression, operation)
            count = len(self.kernel.calls)
            response = self.kernel.cell("await model.progress()")
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_MODEL_INPUT", response["error"])
            self.assertEqual(len(self.kernel.calls), count)
            snapshot = {**model_snapshot(operation, status="running"), "result": batch_progress_result()}
            response = self.kernel.cell("await model.status()", self.reply_with(envelope(snapshot)))
            self.assertFalse(response["ok"])
            self.assertIn("EVAL_MODEL_PROTOCOL", response["error"])

    def test_model_bad_or_lost_acknowledgments_are_never_replayed(self):
        for encoded in ("not JSON", "null", json.dumps(model_ack("judgment")),
                        json.dumps({**model_ack("completion"), "schema": "wrong"}),
                        json.dumps(model_ack("completion", "")),
                        json.dumps(model_ack("completion", timeout_seconds=300)),
                        json.dumps(model_ack("completion", timeout_seconds=True))):
            with self.subTest(encoded=encoded):
                previous = len(self.kernel.calls)
                response = self.kernel.cell("await completion('work')", lambda frame: self.kernel.reply(frame, encoded))
                self.assertFalse(response["ok"])
                self.assertIn("EVAL_MODEL_PROTOCOL", response["error"])
                self.assertEqual(len(self.kernel.calls), previous + 1)
        previous = len(self.kernel.calls)
        response = self.kernel.cell("await completion('work')", lambda frame: self.kernel.reply(frame, error="ACK_LOST"))
        self.assertFalse(response["ok"])
        self.assertIn("ACK_LOST", response["error"])
        self.assertEqual(len(self.kernel.calls), previous + 1)

    def test_model_helpers_reject_unknown_or_authority_options_without_dispatch(self):
        for expression in ("completion('work', model='other')", "completion('work', model='Slow')",
                           "completion('work', model=' smol')", "completion('work', model='provider/model')",
                           "completion('work', model=None)", "completion('work', model=True)",
                           "judge('state', {'q':{}}, model={'role':'slow'})",
                           "judge_batch(['state'], {'q':{}}, model=1)", "completion('work', api_key='secret')",
                           "completion('work', max_tokens=0)", "completion('work', timeout_seconds=601)",
                           "completion('work', timeout_seconds=True)", "judge('state', {'q':{}}, retries=1)",
                           "judge('state', {'q':{}}, operation='other')", "judge_batch(['state'], {'q':{}}, min_ok=1)",
                           "judge_batch(['state'], {'q':{}}, intent='custom')",
                           "judge_batch(['state'], {'q':{}}, concurrency=9)", "judge_batch([], {'q':{}})",
                           "completion.attach('invalid id')"):
            with self.subTest(expression=expression):
                response = self.kernel.cell(f"await {expression}")
                self.assertFalse(response["ok"])
                self.assertIn("EVAL_MODEL_INPUT", response["error"])
        self.assertEqual(self.kernel.calls, [])

    def test_model_roles_are_per_launch_and_never_replayed_or_inherited(self):
        for role in ("default", "smol", "slow", "plan", "commit", "vision", "designer", "task", "advisor", "tiny"):
            with self.subTest(role=role):
                request = self.launch_model(f"completion('work', model={role!r})")
                self.assertEqual(request["model"], role)
        request = self.launch_model("completion('cell binding')")
        self.assertEqual(request, {"prompt": "cell binding"})
        response = self.kernel.cell("await completion('blocked', model='slow')",
                                    lambda frame: self.kernel.reply(frame, error="PI_COMPLETION_BINDING"))
        self.assertFalse(response["ok"])
        self.assertIn("PI_COMPLETION_BINDING", response["error"])
        self.assertEqual(len(self.kernel.calls), 12)

    def test_model_attach_reconstructs_handles_and_checks_current_owner_and_operation(self):
        snapshot = model_snapshot("completion", job_id="recovered", status="running")
        response = self.kernel.cell("model = await completion.attach('recovered'); (model.id, model.job_id, model.operation)",
                                    self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response), ("recovered", "recovered", "completion"))
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"], {
            "name": "jobs", "args": {"action": "wait", "jobId": "recovered", "timeoutMs": 0},
        })
        response = self.kernel.cell("await model.output()", self.reply_with(envelope(snapshot)))
        self.assertTrue(response["ok"], response.get("error"))
        self.assertIsNone(response["result"])
        response = self.kernel.cell("await model.done()", self.reply_with(envelope(
            model_snapshot("completion", model_result("completion", {"schema": "pi.completion.v1", "text": "done"}),
                           job_id="recovered"))))
        self.assertTrue(self.value(response))
        response = self.kernel.cell("await model.status()", lambda frame: self.kernel.reply(frame, error="PI_JOBS_UNKNOWN_ID"))
        self.assertFalse(response["ok"])
        self.assertIn("PI_JOBS_UNKNOWN_ID", response["error"])
        for changed in ({"command": "eval judgment"}, {"command": "eval completion"}, {"command": "reusable subagent pool pool-id"},
                        {"id": "different"}, {"status": "unknown"}):
            response = self.kernel.cell("await completion.attach('recovered')", self.reply_with(envelope({**snapshot, **changed})))
            self.assertFalse(response["ok"])
            self.assertIn("PROTOCOL", response["error"])

    def test_model_timeout_and_cancellation_do_not_fabricate_answers_or_repeat_generation(self):
        self.launch_model()
        response = self.kernel.cell("await model.wait(0)", self.reply_with(envelope(
            model_snapshot("completion", status="running"))))
        self.assertFalse(response["ok"])
        self.assertIn("EVAL_MODEL_TIMEOUT", response["error"])
        cancelled = {"content": [{"type": "text", "text": "cancelled"}], "details": None, "isError": True}
        snapshot = model_snapshot("completion", cancelled, status="killed")
        response = self.kernel.cell("await model.cancel()", self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response)["status"], "killed")
        self.assertEqual(self.kernel.calls[-1]["bridge"]["input"]["args"]["action"], "cancel")
        response = self.kernel.cell("await model.wait(0, raise_errors=False)", self.reply_with(envelope(snapshot)))
        self.assertEqual(self.value(response), cancelled)
        self.assertEqual(sum(frame["bridge"]["tool"] == "__pi_model_complete_start" for frame in self.kernel.calls), 1)

    def test_model_terminal_schema_and_operation_must_match_the_retained_handle(self):
        for native in (model_result("judgment", judgment_value(1)),
                       model_result("completion", {"schema": "pi.completion.v1", "text": "null", "data": None}),
                       model_result("completion", {"schema": "wrong", "text": "answer"}),
                       model_result("completion", None),
                       {"content": [], "details": None}):
            with self.subTest(native=native):
                self.launch_model()
                response = self.kernel.cell("await model.wait()", self.reply_with(envelope(model_snapshot("completion", native))))
                self.assertFalse(response["ok"])
                self.assertIn("EVAL_MODEL_PROTOCOL", response["error"])

    def test_mixed_model_and_agent_waits_use_one_deadline_and_preserve_failed_native_results(self):
        self.launch_agent("agent_job", "agent-native")
        self.launch_model()
        requests = []
        def bridge(frame):
            request = frame["bridge"]["input"]["args"]
            requests.append(request)
            if request["jobId"] == "model-native":
                time.sleep(0.05)
                snapshot = model_snapshot("completion", model_result("completion", None, failed=True,
                                          error="PI_COMPLETION_PROVIDER"), status="failed")
            else:
                snapshot = {"schema": "pi.host_job.v1", "id": "agent-native", "status": "exited",
                            "result": {"content": [], "details": {"answer": 7}}}
            self.kernel.reply(frame, json.dumps(envelope(snapshot)))
        response = self.kernel.cell("await wait([model, agent_job], timeout=10, raise_errors=False)", bridge)
        values = self.value(response)
        self.assertEqual(values[0]["details"]["error"], "PI_COMPLETION_PROVIDER")
        self.assertEqual(values[1]["details"]["answer"], 7)
        self.assertEqual([request["jobId"] for request in requests], ["model-native", "agent-native"])
        self.assertLessEqual(requests[1]["timeoutMs"], requests[0]["timeoutMs"] - 40)

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
