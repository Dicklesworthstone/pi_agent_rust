"""Run the production Python helper against deterministic native-pool receipts.

These are guest protocol tests, not native child-process or provider tests.
"""
import asyncio
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest


SOURCE = Path(__file__).resolve().parents[1] / "src/eval/python_workpool_pipeline.py"


def fixture(**settings):
    spec = importlib.util.spec_from_file_location("pipeline_under_test", SOURCE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    state = SimpleNamespace(time=0, factory_calls=0, tasks=[], reduced=0, high_water=0,
                            events=[], drains=[], close_calls=0, finish_calls=0, options=None)
    module.time = SimpleNamespace(monotonic=lambda: state.time)

    class Pool:
        id = None
        concurrency = settings.get("concurrency", 2)
        sealed = False

        async def push(self, tasks):
            state.events.append("push")
            if self.sealed:
                raise AssertionError("push after sealing")
            handles = []
            for task in tasks:
                state.tasks.append(task)
                self.id = "pool-1"
                item_id = len(state.tasks)
                state.high_water = max(state.high_water, item_id - state.reduced)
                handles.append(SimpleNamespace(item_id=item_id, pool_id=self.id))
            if "push_error" in settings:
                raise settings["push_error"]
            if settings.get("bad_handle"):
                handles[0].item_id = 32
            return handles

        async def finish(self):
            state.events.append("finish")
            state.finish_calls += 1
            self.sealed = True
            return SimpleNamespace(id="job-1")

        async def drain(self, *, cursor, timeout, max_items):
            state.events.append("drain")
            state.drains.append({"cursor": cursor, "timeout": timeout, "max_items": max_items})
            if "drain_error" in settings:
                raise settings["drain_error"]
            before = [] if cursor is None else cursor["itemIds"]
            ready = [item_id for item_id in range(1, len(state.tasks) + 1) if item_id not in before]
            if settings.get("empty_page"):
                chosen = []
            elif settings.get("page_size"):
                chosen = ready[:min(max_items, settings["page_size"])]
            else:
                chosen = ready[-1:]
            items = []
            for item_id in chosen:
                failed = item_id == settings.get("failed_id")
                items.append({"id": item_id, "status": "failed" if failed else "completed", "result": {
                    "content": [{"type": "text", "text": state.tasks[item_id - 1]}],
                    "details": {"results": [{"data": {"task": state.tasks[item_id - 1]}}]},
                    "isError": failed,
                }})
            if items and settings.get("foreign_item"):
                items[0]["id"] = 31
            done = self.sealed and len(before) + len(chosen) == len(state.tasks)
            status = ("failed" if settings.get("failed_id") or settings.get("carrier_failure") else "completed") if done else (
                "draining" if self.sealed else "open")
            return {"schema": "pi.subagent.pool.drain.v1", "poolId": self.id, "items": items,
                    "cursor": {"schema": "pi.subagent.pool.cursor.v1", "poolId": self.id, "itemIds": list(before)},
                    "nextCursor": {"schema": "pi.subagent.pool.cursor.v1", "poolId": self.id,
                                   "itemIds": sorted([*before, *chosen])},
                    "done": done, "status": status, "timedOut": not items and not done}

        async def close(self):
            state.events.append("close")
            state.close_calls += 1
            if "close_error" in settings:
                raise settings["close_error"]
            return {"allClosed": not settings.get("unknown_admission", False),
                    "unknownAdmission": settings.get("unknown_admission", False)}

    pool = Pool()

    async def create_pool(**options):
        state.factory_calls += 1
        state.options = options
        return pool

    kernel = SimpleNamespace(_workpool=create_pool)
    module.install(kernel)
    return SimpleNamespace(pool=pool, state=state, run=create_pool.map_reduce, kernel=kernel)


def mapper(value, index):
    return f"task {index}: {value}"


def reduction(f):
    def reduce(value, item):
        f.state.reduced += 1
        return [*value, {"index": item["index"], "input": item["input"], "status": item["status"],
                        "task": item["result"]["details"]["results"][0]["data"]["task"]}]
    return reduce


class PipelineTests(unittest.IsolatedAsyncioTestCase):
    async def test_streams_async_inputs_with_backpressure_and_full_results(self):
        f = fixture()
        pulled = 0

        async def inputs():
            nonlocal pulled
            for index in range(9):
                self.assertLess(pulled - f.state.reduced, 2)
                pulled += 1
                await asyncio.sleep(0)
                yield f"file-{index}"

        async def map_task(value, index):
            await asyncio.sleep(0)
            return mapper(value, index)

        async def reduce(value, item):
            await asyncio.sleep(0)
            return reduction(f)(value, item)

        published = []
        result = await f.run(inputs(), map_task, reduce, [], pool={"agent": "reviewer", "concurrency": 2},
                             on_pool=published.append)
        self.assertIs(published[0], f.pool)
        self.assertEqual(result["completed"], 9)
        self.assertEqual(result["succeeded"], 9)
        self.assertEqual(result["failed"], 0)
        self.assertEqual(f.state.high_water, 2)
        self.assertEqual(f.state.finish_calls, 1)
        self.assertEqual(f.state.close_calls, 0)
        self.assertNotEqual(result["value"][0]["index"], 0)
        self.assertEqual(sorted(row["index"] for row in result["value"]), list(range(9)))
        self.assertEqual(result["cursor"]["itemIds"], list(range(1, 10)))

    async def test_empty_input_does_not_start_or_finish_native_work(self):
        f = fixture()
        initial = {"count": 0}
        result = await f.run([], mapper, reduction(f), initial)
        self.assertIs(result["value"], initial)
        self.assertEqual(result["status"], "empty")
        self.assertIsNone(result["cursor"])
        self.assertIsNone(f.pool.id)
        self.assertEqual(f.state.events, [])

    async def test_reducer_failure_retains_partial_page_cursor_and_pending_inputs(self):
        f = fixture(concurrency=4, page_size=4)
        cause = ValueError("reducer failed")

        def reduce(value, item):
            if item["id"] == 2:
                raise cause
            f.state.reduced += 1
            return value + 1

        with self.assertRaises(RuntimeError) as caught:
            await f.run(["a", "b", "c"], mapper, reduce, 0)
        error = caught.exception
        self.assertIs(error.__cause__, cause)
        self.assertEqual(error.stage, "reduce")
        self.assertEqual(error.progress["value"], 1)
        self.assertEqual(error.cursor["itemIds"], [1])
        self.assertEqual([entry["input"] for entry in error.pending], ["b", "c"])
        page = await f.pool.drain(cursor=error.cursor, timeout=0, max_items=4)
        self.assertEqual([item["id"] for item in page["items"]], [2, 3])
        self.assertEqual(f.state.close_calls, 1)

    async def test_failed_item_is_not_given_to_success_only_reducer(self):
        f = fixture(concurrency=4, page_size=4, failed_id=2)
        with self.assertRaises(RuntimeError) as caught:
            await f.run(["a", "b"], mapper, reduction(f), [])
        error = caught.exception
        self.assertEqual(error.stage, "result")
        self.assertIn("ITEM_FAILED", str(error.__cause__))
        self.assertEqual(error.cursor["itemIds"], [1])
        self.assertEqual(error.item["status"], "failed")

    async def test_explicit_partial_failure_policy_preserves_failed_results(self):
        f = fixture(page_size=4, failed_id=2)
        result = await f.run(["a", "b", "c"], mapper, reduction(f), [], raise_errors=False)
        self.assertEqual((result["completed"], result["succeeded"], result["failed"]), (3, 2, 1))
        self.assertEqual(result["status"], "failed")
        self.assertEqual(next(row for row in result["value"] if row["index"] == 1)["status"], "failed")
        self.assertEqual(f.state.close_calls, 0)

    async def test_carrier_failure_is_not_silently_successful(self):
        f = fixture(carrier_failure=True)
        with self.assertRaises(RuntimeError) as caught:
            await f.run(["a"], mapper, reduction(f), [])
        self.assertIn("POOL_FAILED", str(caught.exception.__cause__))
        self.assertEqual(caught.exception.progress["completed"], 1)

    async def test_exact_generator_capacity_succeeds(self):
        f = fixture()
        result = await f.run((value for value in ["a", "b", "c"]), mapper, reduction(f), [], max_items=3)
        self.assertEqual(result["completed"], 3)
        self.assertEqual(len(f.state.tasks), 3)

    async def test_capacity_overflow_is_explicit_and_closes_the_source(self):
        f = fixture()
        closed = False
        mapped = 0

        def inputs():
            nonlocal closed
            try:
                yield from range(8)
            finally:
                closed = True

        def map_task(value, index):
            nonlocal mapped
            mapped += 1
            return mapper(value, index)

        with self.assertRaises(RuntimeError) as caught:
            await f.run(inputs(), map_task, reduction(f), [], max_items=3)
        self.assertIn("CAPACITY", str(caught.exception.__cause__))
        self.assertEqual(mapped, 3)
        self.assertTrue(closed)
        self.assertEqual(f.state.close_calls, 1)

    async def test_lost_admission_is_never_retried(self):
        cause = OSError("reply lost after admission")
        f = fixture(push_error=cause, unknown_admission=True)
        with self.assertRaises(RuntimeError) as caught:
            await f.run(["a"], mapper, reduction(f), [])
        error = caught.exception
        self.assertIs(error.__cause__, cause)
        self.assertEqual(error.stage, "admission")
        self.assertEqual(len(f.state.tasks), 1)
        self.assertEqual(error.progress["admitted"], 0)
        self.assertFalse(error.cleanup["allClosed"])
        self.assertTrue(error.cleanup["unknownAdmission"])

    async def test_lost_drain_reply_does_not_advance_cursor(self):
        cause = OSError("read failed")
        f = fixture(drain_error=cause)
        with self.assertRaises(RuntimeError) as caught:
            await f.run(["a", "b"], mapper, reduction(f), [])
        self.assertIs(caught.exception.__cause__, cause)
        self.assertIsNone(caught.exception.cursor)
        self.assertEqual(len(f.state.tasks), 2)

    async def test_shared_timeout_budget_shrinks_between_pages(self):
        f = fixture(concurrency=1)

        def reduce(value, item):
            f.state.time += 6
            f.state.reduced += 1
            return value + 1

        with self.assertRaises(RuntimeError) as caught:
            await f.run(["a", "b", "c"], mapper, reduce, 0, timeout=10)
        self.assertIsInstance(caught.exception.__cause__, TimeoutError)
        self.assertEqual([call["timeout"] for call in f.state.drains], [10, 4])
        self.assertEqual(caught.exception.cursor["itemIds"], [1, 2])
        self.assertEqual(caught.exception.progress["completed"], 2)
        self.assertEqual(len(f.state.tasks), 2)

    async def test_empty_page_does_not_spin(self):
        f = fixture(empty_page=True)
        with self.assertRaises(RuntimeError) as caught:
            await f.run(["a"], mapper, reduction(f), [])
        self.assertIsInstance(caught.exception.__cause__, TimeoutError)
        self.assertEqual(len(f.state.drains), 1)

    async def test_native_close_precedes_iterator_close_and_retains_failures(self):
        primary = ValueError("mapper failed")
        close_error = OSError("native close unavailable")
        return_error = OSError("source cleanup failed")
        f = fixture(close_error=close_error)

        class Inputs:
            def __iter__(self):
                return self

            def __next__(self):
                return "a"

            def close(self):
                f.state.events.append("return")
                raise return_error

        def fail(*_):
            raise primary

        with self.assertRaises(RuntimeError) as caught:
            await f.run(Inputs(), fail, reduction(f), [])
        self.assertIs(caught.exception.__cause__, primary)
        self.assertEqual(f.state.events, ["close", "return"])
        self.assertEqual(caught.exception.cleanup_errors, [close_error, return_error])

    async def test_async_generator_cleanup_is_awaited(self):
        f = fixture()
        closed = False

        async def inputs():
            nonlocal closed
            try:
                yield "a"
                yield "b"
            finally:
                await asyncio.sleep(0)
                closed = True

        with self.assertRaises(RuntimeError):
            await f.run(inputs(), lambda *_: None, reduction(f), [])
        self.assertTrue(closed)
        self.assertEqual(f.state.tasks, [])

    async def test_source_failure_keeps_original_cause(self):
        cause = ValueError("source read failed")
        f = fixture()

        def inputs():
            yield "a"
            raise cause

        with self.assertRaises(RuntimeError) as caught:
            await f.run(inputs(), mapper, reduction(f), [])
        self.assertIs(caught.exception.__cause__, cause)
        self.assertEqual(caught.exception.stage, "source")
        self.assertEqual(len(f.state.tasks), 1)

    async def test_bad_ack_and_foreign_items_fail_closed(self):
        for settings in ({"bad_handle": True}, {"foreign_item": True}):
            f = fixture(**settings)
            with self.assertRaises(RuntimeError) as caught:
                await f.run(["a"], mapper, reduction(f), [])
            self.assertIn("PROTOCOL", str(caught.exception.__cause__))
            self.assertEqual(f.state.reduced, 0)
            self.assertEqual(f.state.close_calls, 1)

    async def test_invalid_options_fail_before_creation(self):
        for options in ({"max_items": 0}, {"max_items": 33}, {"max_items": True},
                        {"timeout": float("nan")}, {"timeout": float("inf")},
                        {"timeout": -1}, {"timeout": 0}, {"timeout": True},
                        {"timeout": 601}, {"raise_errors": 1}, {"on_pool": 7},
                        {"pool": []}, {"misspelled": True}):
            f = fixture()
            with self.assertRaises(TypeError):
                await f.run([], mapper, reduction(f), [], **options)
            self.assertEqual(f.state.factory_calls, 0)
        f = fixture()
        with self.assertRaises(TypeError):
            await f.run(["a", "b"], mapper, reduction(f), [], max_items=1)
        self.assertEqual(f.state.factory_calls, 0)

    async def test_captured_factory_is_not_replaced_by_guest(self):
        f = fixture()
        f.kernel._workpool = lambda **_: self.fail("unexpected factory")
        result = await f.run(["a"], mapper, reduction(f), [])
        self.assertEqual(result["completed"], 1)
        self.assertEqual(f.state.factory_calls, 1)

    async def test_false_none_and_zero_are_valid_accumulators(self):
        for expected in (False, None, 0):
            f = fixture()
            result = await f.run(["a"], mapper, lambda *_: expected, "initial")
            self.assertIs(result["value"], expected)
            self.assertEqual(result["completed"], 1)

    async def test_task_cancellation_closes_accepted_work_and_source_without_wrapping(self):
        f = fixture()
        entered = asyncio.Event()
        closed = False

        async def inputs():
            nonlocal closed
            try:
                yield "a"
                yield "b"
            finally:
                closed = True

        async def map_task(value, index):
            if index == 1:
                entered.set()
                await asyncio.Event().wait()
            return mapper(value, index)

        operation = asyncio.create_task(f.run(inputs(), map_task, reduction(f), []))
        await entered.wait()
        operation.cancel()
        with self.assertRaises(asyncio.CancelledError) as caught:
            await operation
        self.assertTrue(operation.cancelled())
        self.assertTrue(closed)
        self.assertEqual(len(f.state.tasks), 1)
        self.assertEqual(f.state.close_calls, 1)
        self.assertEqual(caught.exception.pending[0]["input"], "a")

    async def test_cancellation_during_cleanup_is_not_swallowed(self):
        cancellation = asyncio.CancelledError("cancel during close")
        primary = ValueError("mapper failed")
        f = fixture(close_error=cancellation)

        def fail(*_):
            raise primary

        with self.assertRaises(asyncio.CancelledError) as caught:
            await f.run(["a"], fail, reduction(f), [])
        self.assertIs(caught.exception, cancellation)
        self.assertIs(caught.exception.__cause__, primary)
        self.assertEqual(caught.exception.pipeline_error.stage, "map")


if __name__ == "__main__":
    unittest.main()
