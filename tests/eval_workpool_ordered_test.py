"""Actual Python helper with a controlled pool, not native-worker acceptance."""
import asyncio
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import unittest


class Fixture:
    def __init__(self, concurrency=3, page_size=1, failed=None, tick=0, choose=None):
        self.tasks, self.calls, self.reduced = [], [], []
        self.closes = self.finishes = self.factories = self.high_water = 0
        self.time = 0
        self.sealed = False
        self.id = None
        self.concurrency = concurrency
        self.page_size, self.failed, self.tick, self.choose = page_size, failed, tick, choose
        path = Path(__file__).resolve().parents[1] / 'src/eval/python_workpool_pipeline.py'
        spec = importlib.util.spec_from_file_location('ordered_pipeline_fixture', path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        module.time = SimpleNamespace(monotonic=lambda: self.time)

        async def factory(**options):
            self.factories += 1
            return self

        module.install(SimpleNamespace(_workpool=factory))
        self.run = factory.map_reduce

    async def push(self, tasks):
        assert not self.sealed
        handles = []
        for task in tasks:
            self.tasks.append(task)
            self.id = 'ordered-pool'
            self.high_water = max(self.high_water, len(self.tasks) - len(self.reduced))
            handles.append(SimpleNamespace(pool_id=self.id, item_id=len(self.tasks)))
        return handles

    async def finish(self):
        self.sealed = True
        self.finishes += 1

    async def close(self):
        self.closes += 1
        return {'allClosed': True}

    async def drain(self, **options):
        self.calls.append(json.loads(json.dumps(options)))
        before = [] if options['cursor'] is None else options['cursor']['itemIds']
        available = [item for item in range(len(self.tasks), 0, -1) if item not in before]
        chosen = (self.choose(available, len(self.calls)) if self.choose else
                  available[:min(self.page_size, options['max_items'])])
        self.time += self.tick
        items = [{'id': item, 'status': 'failed' if item == self.failed else 'completed',
                  'result': {'content': [{'type': 'image', 'data': 'opaque-image'},
                                         {'type': 'text', 'text': self.tasks[item - 1]}],
                             'details': {'id': item, 'task': self.tasks[item - 1]},
                             'isError': item == self.failed}} for item in chosen]
        done = self.sealed and len(before) + len(chosen) == len(self.tasks)
        return {'poolId': self.id, 'items': items, 'done': done,
                'status': ('failed' if self.failed else 'completed') if done else 'open'}

    async def reduce(self, value, item):
        await asyncio.sleep(0)
        self.reduced.append(item['id'])
        return [*value, item]


def mapper(value, index):
    return f'task {index}: {value}'


class OrderedWorkpoolTests(unittest.IsolatedAsyncioTestCase):
    async def test_async_source_and_reducer_with_bounded_worker_windows(self):
        for concurrency in (1, 2, 3, 8):
            with self.subTest(concurrency=concurrency):
                f = Fixture(concurrency=concurrency)
                pulled = 0

                async def inputs():
                    nonlocal pulled
                    for index in range(17):
                        self.assertLess(pulled - len(f.reduced), concurrency)
                        pulled += 1
                        await asyncio.sleep(0)
                        yield f'source-{index}'

                result = await f.run(inputs(), mapper, f.reduce, [], ordered=True)
                self.assertEqual(f.reduced, list(range(1, 18)))
                self.assertEqual(result['completed'], 17)
                self.assertEqual(result['succeeded'], 17)
                self.assertEqual(result['failed'], 0)
                self.assertEqual(result['status'], 'completed')
                self.assertEqual(f.high_water, concurrency)
                self.assertEqual((f.finishes, f.closes), (1, 0))
                for index, item in enumerate(result['value']):
                    self.assertEqual(item['index'], index)
                    self.assertEqual(item['input'], f'source-{index}')
                    self.assertEqual(item['result']['content'][0]['data'], 'opaque-image')
                    self.assertEqual(item['result']['details']['id'], index + 1)
                self.assertEqual(result['cursor']['itemIds'], f.reduced)

    async def test_default_and_explicit_false_keep_arrival_order(self):
        for options in ({}, {'ordered': False}):
            f = Fixture()
            result = await f.run(['a', 'b', 'c'], mapper, f.reduce, [], **options)
            self.assertEqual(f.reduced, [3, 2, 1])
            self.assertEqual(result['completed'], 3)

    async def test_reversed_same_page_reduces_without_more_admission(self):
        f = Fixture(concurrency=4, page_size=4)
        result = await f.run(['a', 'b', 'c'], mapper, f.reduce, [], ordered=True)
        self.assertEqual(f.reduced, [1, 2, 3])
        self.assertEqual(len(f.calls), 1)
        self.assertEqual(result['status'], 'completed')

    async def test_buffered_reducer_failure_keeps_exact_recovery_cursor(self):
        f = Fixture(concurrency=4, page_size=4)
        cause = RuntimeError('item two could not be persisted')

        async def reduce(value, item):
            if item['id'] == 2:
                raise cause
            f.reduced.append(item['id'])
            return value + 1

        with self.assertRaises(RuntimeError) as caught:
            await f.run(['a', 'b', 'c'], mapper, reduce, 0, ordered=True)
        error = caught.exception
        self.assertIs(error.__cause__, cause)
        self.assertEqual(error.stage, 'reduce')
        self.assertEqual(error.item['id'], 2)
        self.assertEqual(error.cursor['itemIds'], [1])
        self.assertEqual(error.progress['value'], 1)
        self.assertEqual(error.progress['completed'], 1)
        self.assertEqual([item['itemId'] for item in error.pending], [2, 3])
        self.assertEqual(f.closes, 1)
        page = await f.drain(cursor=error.cursor, timeout=1, max_items=4)
        self.assertEqual([item['id'] for item in page['items']], [3, 2])

    async def test_failed_items_follow_source_order_and_failure_policy(self):
        f = Fixture(failed=2)
        with self.assertRaises(RuntimeError) as caught:
            await f.run(['a', 'b', 'c'], mapper, f.reduce, [], ordered=True)
        self.assertEqual(caught.exception.stage, 'result')
        self.assertEqual(caught.exception.item['id'], 2)
        self.assertEqual(caught.exception.cursor['itemIds'], [1])
        self.assertEqual(f.reduced, [1])
        self.assertEqual(f.closes, 1)
        f = Fixture(failed=2)
        result = await f.run(['a', 'b', 'c'], mapper, f.reduce, [],
                             ordered=True, raise_errors=False)
        self.assertEqual(f.reduced, [1, 2, 3])
        self.assertEqual((result['succeeded'], result['failed']), (2, 1))
        self.assertEqual(result['status'], 'failed')

    async def test_private_receive_cursor_does_not_acknowledge_on_timeout(self):
        f = Fixture(tick=4)
        with self.assertRaises(RuntimeError) as caught:
            await f.run(['a', 'b', 'c'], mapper, f.reduce, [], ordered=True, timeout=10)
        error = caught.exception
        self.assertIsInstance(error.__cause__, TimeoutError)
        self.assertEqual([call['timeout'] for call in f.calls], [10, 6, 2])
        self.assertEqual([call['cursor']['itemIds'] if call['cursor'] else []
                          for call in f.calls], [[], [3], [2, 3]])
        self.assertIsNone(error.cursor)
        self.assertEqual(error.progress['completed'], 0)
        self.assertEqual(f.reduced, [])
        self.assertEqual((len(f.tasks), f.closes), (3, 1))

    async def test_cancellation_preserves_type_and_buffered_results(self):
        f = Fixture()
        cancellation = asyncio.CancelledError('stop the ordered pipeline')
        closed = False

        async def inputs():
            nonlocal closed
            try:
                for value in ('a', 'b', 'c', 'd'):
                    yield value
            finally:
                closed = True

        async def reduce(value, item):
            if item['id'] == 2:
                raise cancellation
            f.reduced.append(item['id'])
            return value + 1

        with self.assertRaises(asyncio.CancelledError) as caught:
            await f.run(inputs(), mapper, reduce, 0, ordered=True)
        error = caught.exception
        self.assertIs(error, cancellation)
        self.assertEqual(error.cursor['itemIds'], [1])
        self.assertEqual([item['itemId'] for item in error.pending], [2, 3])
        self.assertEqual(error.progress['completed'], 1)
        self.assertEqual((f.closes, len(f.tasks)), (1, 3))
        self.assertTrue(closed)

    async def test_duplicate_buffered_receipt_fails_without_polling_forever(self):
        f = Fixture(choose=lambda available, call: [3])
        with self.assertRaises(RuntimeError) as caught:
            await f.run(['a', 'b', 'c'], mapper, f.reduce, [], ordered=True)
        self.assertIn('PROTOCOL', str(caught.exception.__cause__))
        self.assertIsNone(caught.exception.cursor)
        self.assertEqual((len(f.calls), f.closes), (2, 1))

    async def test_ordered_requires_bool_before_pool_creation(self):
        for ordered in (None, 0, 1, 'true', [], {}):
            f = Fixture()
            with self.assertRaises(TypeError):
                await f.run([], mapper, f.reduce, [], ordered=ordered)
            self.assertEqual(f.factories, 0)

    async def test_ordered_empty_input_keeps_false_accumulator(self):
        f = Fixture()
        result = await f.run([], mapper, f.reduce, False, ordered=True)
        self.assertIs(result['value'], False)
        self.assertEqual(result['status'], 'empty')
        self.assertIsNone(result['cursor'])
        self.assertEqual(f.tasks, [])
        self.assertEqual(f.finishes, 0)

    async def test_full_capacity_is_not_silently_truncated(self):
        f = Fixture(concurrency=8, page_size=4)
        result = await f.run(iter(range(32)), mapper, f.reduce, [], ordered=True)
        self.assertEqual(f.reduced, list(range(1, 33)))
        self.assertEqual(result['completed'], 32)
        self.assertEqual(f.high_water, 8)


if __name__ == '__main__':
    unittest.main()
