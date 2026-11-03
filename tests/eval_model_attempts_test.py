"""Shipped Python kernel with controlled native replies, not Rust acceptance.

The existing protocol harness starts the real server. Every observation passes
through a new bridge exchange; these tests do not simulate provider execution.
"""
import ast
import copy
import json
import unittest

from eval_python_bridge import Kernel


def receipt(attempt, status, operation="completion"):
    return {"attempt": attempt, "operation": operation, "status": status,
            "provider": "fixture", "modelId": f"model-{attempt}",
            "model": f"fixture/model-{attempt}", "api": "controlled", "thinkingLevel": None,
            "usage": None, "usageKnown": False,
            **({"error": "model request failed", "fallbackScheduled": True} if status == "failed" else {})}


def progress(attempts, operation="completion", interrupted=False):
    return {"content": [], "isError": interrupted, "details": {
        "schema": "pi.host_job.interrupted.v1" if interrupted else "pi.host_job.progress.v1",
        "partialResult": {"content": [], "isError": False, "details": {
            "schema": "pi.eval.model-job.attempts.v1", "operation": operation, "complete": False,
            "judgments": {"schema": "pi.eval.judgments.v1", "attempts": attempts}}}}}


def settled(attempts, operation="completion", failed=False):
    payload = ({"schema": "pi.completion.v1", "text": "ready"} if operation == "completion"
               else {"schema": "pi.judgment.v1", "answers": {"safe": False, "score": 0}})
    return {"content": [{"type": "text", "text": "model request failed" if failed else "ready"}],
            "isError": failed, "details": {
                "schema": "pi.eval.model-job.result.v1", "operation": operation,
                "result": None if failed else payload,
                "judgments": {"schema": "pi.eval.judgments.v1", "attempts": attempts},
                **({"error": "model request failed"} if failed else {})}}


class ModelAttemptTests(unittest.TestCase):
    def setUp(self):
        self.kernel = Kernel()
        self.addCleanup(self.kernel.close)
        self.failure = None
        self.snapshot = {"schema": "pi.host_job.v1", "id": "job-a",
                         "command": "eval model completion", "status": "running", "result": None}
        self.run_cell("job = await completion.attach('job-a')")
        self.kernel.calls.clear()

    def reply(self, frame):
        call = frame["bridge"]
        self.assertEqual(call["tool"], "__pi_tool_result")
        self.assertEqual(call["input"]["name"], "jobs")
        result = {"schema": "pi.eval.tool_result.v1", "isError": self.failure is not None,
                  "content": [{"type": "text", "text": self.failure}] if self.failure else [],
                  "details": None if self.failure else copy.deepcopy(self.snapshot)}
        return {"bridge_result": {"call": call["call"], "ok": True, "content": json.dumps(result)}}

    def run_cell(self, code):
        response, _ = self.kernel.cell(code, self.reply)
        self.assertTrue(response["ok"], response.get("error"))
        self.assertNotIn("bridgeError", response)
        return ast.literal_eval(response["result"]) if response.get("result") is not None else None

    def assert_error(self, code, message):
        response, _ = self.kernel.cell(code, self.reply)
        self.assertFalse(response["ok"])
        self.assertIn(message, response["error"])

    def test_no_receipt_yet_makes_one_current_host_observation(self):
        self.assertEqual(self.run_cell("await job.attempts()"), [])
        self.assertEqual(len(self.kernel.calls), 1)
        self.assertEqual(self.kernel.calls[0]["bridge"]["input"], {
            "name": "jobs", "args": {"action": "wait", "jobId": "job-a", "timeoutMs": 0}})

    def test_running_receipts_preserve_order_provenance_and_unknown_usage(self):
        attempts = [receipt(5, "failed"), receipt(6, "running")]
        self.snapshot["result"] = progress(attempts)
        self.assertEqual(self.run_cell("await job.attempts()"), attempts)
        self.assertEqual(len(self.kernel.calls), 1)

    def test_settled_completion_and_judgment_keep_values_and_history(self):
        for operation in ("completion", "judgment"):
            self.snapshot["command"] = "eval model completion" if operation == "completion" else "eval judgment"
            self.snapshot.update(status="running", result=None)
            helper = "completion" if operation == "completion" else "judge"
            self.run_cell(f"job = await {helper}.attach('job-a')")
            attempts = [receipt(1, "failed", operation), receipt(2, "completed", operation)]
            self.snapshot.update(status="exited", result=settled(attempts, operation))
            self.assertEqual(self.run_cell("await job.attempts()"), attempts)
            self.assertEqual(self.run_cell("await job.wait()"),
                             "ready" if operation == "completion" else {"safe": False, "score": 0})

    def test_failed_terminal_receipts_do_not_change_wait_failure(self):
        attempts = [receipt(1, "failed")]
        self.snapshot.update(status="failed", result=settled(attempts, failed=True))
        self.assertEqual(self.run_cell("await job.attempts()"), attempts)
        self.assert_error("await job.wait()", "model request failed")

    def test_interrupted_receipts_do_not_fabricate_completion(self):
        for status in ("killed", "timedOut"):
            attempts = [receipt(1, "running")]
            self.snapshot.update(status=status, result=progress(attempts, interrupted=True))
            self.assertEqual(self.run_cell("await job.attempts()"), attempts)
            self.assert_error("await job.wait()", status)

    def test_each_observation_rechecks_policy_without_cached_receipts(self):
        self.snapshot["result"] = progress([receipt(1, "running")])
        self.run_cell("await job.attempts()")
        self.failure = "current jobs access revoked"
        self.assert_error("await job.attempts()", self.failure)
        self.assertEqual(len(self.kernel.calls), 2)

    def test_malformed_running_receipts_are_rejected(self):
        invalid = [[], [None], [receipt(True, "running")], [receipt(1, "queued")],
                   [receipt(1, "failed"), receipt(3, "running")], [receipt(1, "running", "judgment")],
                   [{**receipt(1, "running"), "usageKnown": "false"}],
                   [receipt(i + 1, "running") for i in range(17)]]
        for attempts in invalid:
            with self.subTest(attempts=attempts):
                self.snapshot["result"] = progress(attempts)
                self.assert_error("await job.attempts()", "EVAL_MODEL_PROTOCOL")

    def test_malformed_terminal_receipts_and_foreign_jobs_are_rejected(self):
        self.snapshot["status"] = "exited"
        for attempts in ([None], {}, [receipt(i + 1, "completed") for i in range(17)]):
            self.snapshot["result"] = settled(attempts)
            self.assert_error("await job.attempts()", "EVAL_MODEL_PROTOCOL")
        self.snapshot.update(id="foreign-job", result=settled([receipt(1, "completed")]))
        self.assert_error("await job.attempts()", "EVAL_AGENT_PROTOCOL")


if __name__ == "__main__":
    unittest.main()
