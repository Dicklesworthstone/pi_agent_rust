"""Production recovery installer over controlled kernel handles, not Rust acceptance.

The inherited kernel methods below are an explicit test double. Native-kernel
integration scenarios live in eval_agent_recovery.rs and require DSR. These
checks isolate recovery/admission behavior without a child process or provider.
"""
import asyncio
import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest

SOURCE = Path(__file__).resolve().parents[1] / 'src/eval/python_agent_recovery.py'
spec = importlib.util.spec_from_file_location('recovery_under_test', SOURCE)
MODULE = importlib.util.module_from_spec(spec)
spec.loader.exec_module(MODULE)


def answer(text):
    return {'content': [{'type': 'text', 'text': text}], 'details': {'answer': text}, 'isError': False}


def worker(turn=3, status='completed', **extra):
    return {'schema': 'pi.subagent.worker.v1', 'workerId': 'worker-a', 'jobId': 'job-a',
            'agent': 'reviewer', 'status': 'idle', 'turnId': turn,
            'turn': {'id': turn, 'status': status,
                     'result': None if status in ('queued', 'running') else answer(f'turn-{turn}')}, **extra}


def pool(states=('completed', 'running'), status='open', **extra):
    return {'schema': 'pi.subagent.pool.v1', 'poolId': 'pool-a', 'jobId': 'job-pool',
            'agent': 'reviewer', 'concurrency': 2, 'sealed': status != 'open', 'status': status,
            'items': [{'id': index + 1, 'status': state} for index, state in enumerate(states)], **extra}


def fixture():
    kernel = SimpleNamespace(calls=[], owner=SimpleNamespace(active=True, bridge_open=True))
    class BridgeError(RuntimeError):
        pass
    kernel.ToolBridgeError = BridgeError
    async def respond(name, args):
        if args['operation'].startswith('worker_'):
            return worker(args.get('turnId', 3))
        return pool()
    kernel.respond = respond
    async def native(name, args):
        kernel.calls.append((name, dict(args)))
        return await kernel.respond(name, args)
    kernel._agent_native_result = native
    kernel._bridge_owner = lambda: kernel.owner
    def check_worker(value, worker_id=None, turn_id=None):
        if (value.get('schema') != 'pi.subagent.worker.v1'
                or worker_id is not None and value['workerId'] != worker_id
                or turn_id is not None and value['turnId'] != turn_id):
            raise BridgeError('EVAL_AGENT_PROTOCOL: bad worker')
        return value
    kernel._agent_worker_snapshot = check_worker
    def check_pool(value, pool_id=None):
        if value.get('schema') != 'pi.subagent.pool.v1' or pool_id is not None and value['poolId'] != pool_id:
            raise BridgeError('EVAL_POOL_PROTOCOL: bad pool')
        return value
    kernel._pool_snapshot = check_pool
    kernel._pool_bounded_text = lambda value, maximum: (
        type(value) is str and bool(value.strip()) and '\0' not in value and len(value.encode()) <= maximum)

    class Turn:
        def __init__(self, parent, turn_id):
            self.worker_id, self.turn_id = parent.id, turn_id
            self.job_id, self.agent = parent.job_id, parent.agent
        async def output(self):
            return (await read_worker(self, 'worker_status'))['turn']['result']
    async def read_worker(turn, operation):
        value = check_worker(await native('subagent', {
            'operation': operation, 'workerId': turn.worker_id, 'turnId': turn.turn_id,
        }), turn.worker_id, turn.turn_id)
        if value['jobId'] != turn.job_id or value['agent'] != turn.agent:
            raise BridgeError('EVAL_AGENT_PROTOCOL: identity changed')
        return value
    kernel._AgentTurn, kernel._agent_read_worker = Turn, read_worker

    class BaseWorker:
        def __init__(self, value):
            self.id, self.job_id, self.agent = value['workerId'], value['jobId'], value['agent']
            self._turn_id = value['turnId']
        @property
        def turn_id(self):
            return self._turn_id
        async def send(self, prompt):
            previous = self._turn_id
            value = check_worker(await native('subagent', {
                'operation': 'worker_send', 'workerId': self.id, 'task': prompt,
            }), self.id)
            if value['jobId'] != self.job_id or value['agent'] != self.agent or value['turnId'] <= previous:
                raise BridgeError('EVAL_AGENT_PROTOCOL: bad send acknowledgment')
            self._turn_id = max(self._turn_id, value['turnId'])
            return Turn(self, value['turnId'])
        async def output(self):
            return await Turn(self, self._turn_id).output()
    class BasePool:
        def __init__(self, options, name, concurrency):
            self._options, self.name, self.concurrency = options, name, concurrency
            self._closed, self._finish_acknowledged = False, False
            self._items, self._pool_id, self._job_id = [], None, None
        @property
        def items(self):
            return tuple(self._items)
        @property
        def closed(self):
            return self._closed
        def _adopt(self, value):
            check_pool(value, self._pool_id)
            if value['agent'] != self._options['agent'] or value['concurrency'] != self.concurrency:
                raise BridgeError('EVAL_POOL_PROTOCOL: identity changed')
            self._pool_id, self._job_id = value['poolId'], value['jobId']
            self._items = list(value['items'])
            self._closed |= value['sealed']
        async def push(self, tasks):
            if self._closed:
                raise BridgeError('EVAL_POOL_CLOSED')
            self._adopt(await native('subagent', {'operation': 'pool_push', 'poolId': self._pool_id, 'tasks': tasks}))
        async def finish(self):
            self._closed = True
            if not self._finish_acknowledged:
                self._adopt(await native('subagent', {'operation': 'pool_finish', 'poolId': self._pool_id}))
                self._finish_acknowledged = True
            return self._job_id
    kernel._AgentWorker, kernel._AgentPool = BaseWorker, BasePool
    async def agent(prompt, **options):
        return kernel._AgentWorker(await native('subagent', {'operation': 'worker_start', 'task': prompt}))
    async def workpool(**options):
        return kernel._AgentPool({'agent': 'reviewer'}, 'workpool', 2)
    kernel._agent, kernel._workpool = agent, workpool
    MODULE.install(kernel)
    return kernel


class RecoveryTests(unittest.IsolatedAsyncioTestCase):
    async def test_inspection_only_worker_no_launch(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a')
        self.assertEqual((w.turn_id, w.recovered, w.send_allowed, w.unknown_admission), (3, True, False, False))
        with self.assertRaisesRegex(k.ToolBridgeError, 'RECOVERY_REQUIRED'):
            await w.send('not admitted')
        self.assertEqual(k.calls, [('subagent', {'operation': 'worker_status', 'workerId': 'worker-a'})])

    async def test_fixed_turn_selection_does_not_change_latest(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a', turn_id=1)
        turn = await w.turn(2)
        self.assertEqual((w.turn_id, turn.turn_id), (1, 2))
        self.assertEqual((await turn.output())['details']['answer'], 'turn-2')

    async def test_explicit_send_only_new_prompt(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a', allow_send=True)
        async def respond(name, args):
            return worker(4, 'queued')
        k.respond = respond
        result = await w.send('new task')
        self.assertEqual((w.turn_id, result.turn_id), (4, 4))
        self.assertEqual(k.calls[-1][1], {'operation': 'worker_send', 'workerId': 'worker-a', 'task': 'new task'})

    async def test_ordinary_launch_also_fences_a_lost_reply(self):
        k = fixture()
        w = await k._agent('seed', keep_alive=True)
        self.assertFalse(w.recovered)
        async def refused(name, args):
            raise k.ToolBridgeError('lost native reply')
        k.respond = refused
        with self.assertRaisesRegex(k.ToolBridgeError, 'lost native reply'):
            await w.send('possibly admitted')
        with self.assertRaisesRegex(k.ToolBridgeError, 'RECOVERY_REQUIRED'):
            await w.send('must not replay')
        self.assertTrue(w.unknown_admission)
        self.assertEqual(len(k.calls), 2)

    async def test_malformed_acknowledgment_fences_without_retry(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a', allow_send=True)
        async def bad(name, args):
            return worker(4, jobId='wrong')
        k.respond = bad
        with self.assertRaisesRegex(k.ToolBridgeError, 'PROTOCOL'):
            await w.send('possibly admitted')
        self.assertFalse(w.send_allowed)
        self.assertTrue(w.unknown_admission)
        self.assertEqual(len(k.calls), 2)

    async def test_cancellation_preserves_exception_and_fences(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a', allow_send=True)
        entered = asyncio.Event()
        async def pending(name, args):
            entered.set()
            await asyncio.Future()
        k.respond = pending
        task = asyncio.create_task(w.send('pending'))
        await entered.wait()
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        self.assertFalse(w.send_allowed)
        self.assertTrue(w.unknown_admission)
        self.assertEqual(w._pending_sends, {})

    async def test_closed_cell_scope_marks_pending_admission_uncertain(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a', allow_send=True)
        entered = asyncio.Event()
        async def pending(name, args):
            entered.set()
            await asyncio.Future()
        k.respond = pending
        task = asyncio.create_task(w.send('unfinished'))
        await entered.wait()
        k.owner.bridge_open = False
        self.assertFalse(w.send_allowed)
        self.assertTrue(w.unknown_admission)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task

    async def test_successful_peer_does_not_clear_uncertainty(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a', allow_send=True)
        gate, entered = asyncio.Event(), asyncio.Event()
        async def respond(name, args):
            if args['task'] == 'lost':
                raise k.ToolBridgeError('lost')
            entered.set()
            await gate.wait()
            return worker(4, 'queued')
        k.respond = respond
        pending = asyncio.create_task(w.send('accepted'))
        await entered.wait()
        with self.assertRaisesRegex(k.ToolBridgeError, 'lost'):
            await w.send('lost')
        gate.set()
        self.assertEqual((await pending).turn_id, 4)
        self.assertEqual((w.send_allowed, w.unknown_admission), (False, True))

    async def test_out_of_order_acks_retain_newest_selection(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a', allow_send=True)
        ready = asyncio.Event()
        requests = {}
        async def respond(name, args):
            future = asyncio.get_running_loop().create_future()
            requests[args['task']] = future
            if len(requests) == 2:
                ready.set()
            return await future
        k.respond = respond
        left, right = asyncio.create_task(w.send('left')), asyncio.create_task(w.send('right'))
        await ready.wait()
        requests['right'].set_result(worker(5, 'queued'))
        self.assertEqual((await right).turn_id, 5)
        requests['left'].set_result(worker(4, 'queued'))
        self.assertEqual((await left).turn_id, 4)
        self.assertEqual((w.turn_id, w.send_allowed), (5, True))

    async def test_invalid_input_does_not_dispatch_or_disable_a_handle(self):
        k = fixture()
        for value in ('', 'white space', '\0', 'x' * 129, '\ud800', 1, False, []):
            with self.assertRaises(TypeError):
                await k._agent.attach_worker(value)
            with self.assertRaises(TypeError):
                await k._workpool.attach(value)
        for options in ({'allow_send': 1}, {'turn_id': True}, {'turn_id': 0}, {'turn_id': 33}, {'model': 'other'}):
            with self.assertRaises(TypeError):
                await k._agent.attach_worker('worker-a', **options)
        self.assertEqual(k.calls, [])
        w = await k._agent.attach_worker('worker-a', allow_send=True)
        for prompt in (None, 1, '', '  '):
            with self.assertRaises(TypeError):
                await w.send(prompt)
        self.assertTrue(w.send_allowed)
        self.assertEqual(len(k.calls), 1)

    async def test_identity_mismatch_refuses_no_fallback(self):
        k = fixture()
        async def wrong(name, args):
            return worker(workerId='other')
        k.respond = wrong
        with self.assertRaisesRegex(k.ToolBridgeError, 'PROTOCOL'):
            await k._agent.attach_worker('worker-a')
        self.assertEqual(len(k.calls), 1)

    async def test_current_policy_is_used_after_recovery(self):
        k = fixture()
        w = await k._agent.attach_worker('worker-a')
        async def denied(name, args):
            raise k.ToolBridgeError('policy revoked')
        k.respond = denied
        with self.assertRaisesRegex(k.ToolBridgeError, 'policy revoked'):
            await w.output()
        self.assertEqual(len(k.calls), 2)

    async def test_pool_recovery_reads_complete_manifest_no_start(self):
        k = fixture()
        p = await k._workpool.attach('pool-a')
        self.assertTrue(p.recovered)
        self.assertTrue(p.closed)
        self.assertEqual([item['id'] for item in p.items], [1, 2])
        with self.assertRaisesRegex(k.ToolBridgeError, 'POOL_CLOSED'):
            await p.push(['do not launch'])
        self.assertEqual(k.calls, [('subagent', {'operation': 'pool_status', 'poolId': 'pool-a'})])

    async def test_pool_explicit_push_uses_only_existing_identity(self):
        k = fixture()
        p = await k._workpool.attach('pool-a', allow_push=True)
        await p.push(['new item'])
        self.assertEqual(k.calls[-1][1], {'operation': 'pool_push', 'poolId': 'pool-a', 'tasks': ['new item']})
        self.assertFalse(any(call[1]['operation'] == 'pool_start' for call in k.calls))

    async def test_sealed_pool_finish_never_resends(self):
        for status in ('draining', 'completed', 'failed', 'cancelled'):
            k = fixture()
            async def sealed(name, args):
                return pool(status=status)
            k.respond = sealed
            p = await k._workpool.attach('pool-a', allow_push=True)
            self.assertTrue(p.closed)
            self.assertEqual(await p.finish(), 'job-pool')
            self.assertEqual(len(k.calls), 1)

    async def test_pool_options_and_bad_identity_are_rejected(self):
        k = fixture()
        for options in ({'allow_push': 1}, {'timeout_seconds': 2}, {'agent': 'other'}):
            with self.assertRaises(TypeError):
                await k._workpool.attach('pool-a', **options)
        self.assertEqual(k.calls, [])
        async def invalid(name, args):
            return pool(jobId='invalid job')
        k.respond = invalid
        with self.assertRaisesRegex(k.ToolBridgeError, 'PROTOCOL'):
            await k._workpool.attach('pool-a')

    async def test_installation_is_once_and_new_factories_use_concrete_types(self):
        k = fixture()
        worker_type, pool_type = k._AgentWorker, k._AgentPool
        MODULE.install(k)
        self.assertIs(k._AgentWorker, worker_type)
        self.assertIs(k._AgentPool, pool_type)
        self.assertIsInstance(await k._agent('seed'), worker_type)
        p = await k._workpool()
        self.assertIsInstance(p, pool_type)
        self.assertFalse(p.recovered)

    async def test_fresh_kernel_recovery_uses_no_guest_history(self):
        old = fixture()
        w = await old._agent.attach_worker('worker-a')
        fresh = fixture()
        result = await fresh._agent.attach_worker(w.id)
        self.assertEqual((result.turn_id, result.send_allowed), (3, False))
        self.assertEqual(fresh.calls[0][1]['operation'], 'worker_status')


if __name__ == '__main__':
    unittest.main()
