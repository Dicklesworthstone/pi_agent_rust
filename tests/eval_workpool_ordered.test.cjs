'use strict';
// Executes the production bootstrap. The native pool is a controlled protocol
// fixture: these tests do not claim QuickJS, Rust or live-agent acceptance.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../src/eval/js_kernel/workpool_pipeline.js'), 'utf8');
const plain = value => JSON.parse(JSON.stringify(value));

function fixture(options = {}) {
    const state = {tasks: [], calls: [], reduced: [], closes: 0, finishes: 0,
        factories: 0, highWater: 0, time: 0, sealed: false};
    const pool = {
        id: null,
        concurrency: options.concurrency ?? 3,
        async push(tasks) {
            assert.equal(state.sealed, false);
            return tasks.map(task => {
                state.tasks.push(task);
                this.id = 'ordered-pool';
                state.highWater = Math.max(state.highWater, state.tasks.length - state.reduced.length);
                return {poolId: this.id, itemId: state.tasks.length};
            });
        },
        async finish() { state.sealed = true; state.finishes++; },
        async close() { state.closes++; return {allClosed: true}; },
        async drain(args) {
            state.calls.push(plain(args));
            const before = args.cursor?.itemIds ?? [];
            let available = state.tasks.map((_, i) => i + 1).filter(id => !before.includes(id));
            available.reverse();
            const chosen = options.choose
                ? options.choose(available, state.calls.length)
                : available.slice(0, Math.min(args.maxItems, options.pageSize ?? 1));
            if (options.tick) state.time += options.tick;
            const items = chosen.map(id => ({id, status: id === options.failed ? 'failed' : 'completed', result: {
                content: [{type: 'image', mimeType: 'image/png', data: 'opaque-image'},
                    {type: 'text', text: state.tasks[id - 1]}],
                details: {results: [{data: {id, task: state.tasks[id - 1]}}]},
                isError: id === options.failed,
            }}));
            const done = state.sealed && before.length + chosen.length === state.tasks.length;
            return {poolId: this.id, items, done,
                status: done ? (options.failed ? 'failed' : 'completed') : 'open'};
        },
    };
    const factory = async () => { state.factories++; return pool; };
    vm.runInNewContext(source, {workpool: factory, Date: {now: () => state.time}});
    return {run: factory.mapReduce, pool, state};
}
const mapTask = (input, index) => `task ${index}: ${input}`;
const reducer = f => async (value, item) => {
    await Promise.resolve();
    f.state.reduced.push(item.id);
    return [...value, item];
};

for (const concurrency of [1, 2, 3, 8]) {
    test(`ordered async source/reducer stays bounded with ${concurrency} workers`, async () => {
        const f = fixture({concurrency});
        let pulled = 0;
        async function* inputs() {
            for (let i = 0; i < 17; i++) {
                assert.ok(pulled - f.state.reduced.length < concurrency);
                pulled++;
                await Promise.resolve();
                yield `source-${i}`;
            }
        }
        const result = await f.run(inputs(), mapTask, reducer(f), [], {ordered: true});
        assert.deepEqual(f.state.reduced, Array.from({length: 17}, (_, i) => i + 1));
        assert.equal(result.completed, 17);
        assert.equal(result.succeeded, 17);
        assert.equal(result.failed, 0);
        assert.equal(result.status, 'completed');
        assert.equal(f.state.highWater, concurrency);
        assert.equal(f.state.finishes, 1);
        assert.equal(f.state.closes, 0);
        for (let i = 0; i < 17; i++) {
            assert.equal(result.value[i].index, i);
            assert.equal(result.value[i].input, `source-${i}`);
            assert.equal(result.value[i].result.content[0].data, 'opaque-image');
            assert.equal(result.value[i].result.details.results[0].data.id, i + 1);
        }
        assert.deepEqual(plain(result.cursor.itemIds), f.state.reduced);
    });
}

test('default and explicit false preserve arrival-order reductions', async () => {
    for (const options of [{}, {ordered: false}]) {
        const f = fixture();
        const result = await f.run(['a', 'b', 'c'], mapTask, reducer(f), [], options);
        assert.deepEqual(f.state.reduced, [3, 2, 1]);
        assert.equal(result.completed, 3);
    }
});

test('reverse same-page results reduce in source order without new admission', async () => {
    const f = fixture({concurrency: 4, pageSize: 4});
    const result = await f.run(['a', 'b', 'c'], mapTask, reducer(f), [], {ordered: true});
    assert.deepEqual(f.state.reduced, [1, 2, 3]);
    assert.equal(f.state.calls.length, 1);
    assert.equal(result.status, 'completed');
});

test('buffered reducer failure exposes only successful acknowledgments for replay', async () => {
    const f = fixture({concurrency: 4, pageSize: 4});
    const cause = new Error('persisting item two failed');
    const error = await f.run(['a', 'b', 'c'], mapTask, async (value, item) => {
        if (item.id === 2) throw cause;
        f.state.reduced.push(item.id);
        return value + 1;
    }, 0, {ordered: true}).catch(error => error);
    assert.equal(error.name, 'WorkpoolPipelineError');
    assert.equal(error.cause, cause);
    assert.equal(error.stage, 'reduce');
    assert.equal(error.item.id, 2);
    assert.equal(error.progress.value, 1);
    assert.equal(error.progress.completed, 1);
    assert.deepEqual(plain(error.cursor.itemIds), [1]);
    assert.deepEqual(plain(error.pending.map(item => item.itemId)), [2, 3]);
    assert.equal(f.state.closes, 1);
    const recovery = await f.pool.drain({cursor: error.cursor, timeout: 1, maxItems: 4});
    assert.deepEqual(recovery.items.map(item => item.id), [3, 2]);
});

test('buffered item failure is handled in source order with truthful error policy', async () => {
    for (const raiseErrors of [true, false]) {
        const f = fixture({failed: 2});
        const result = await f.run(['a', 'b', 'c'], mapTask, reducer(f), [],
            {ordered: true, raiseErrors}).catch(error => error);
        if (raiseErrors) {
            assert.equal(result.stage, 'result');
            assert.equal(result.item.id, 2);
            assert.deepEqual(plain(result.cursor.itemIds), [1]);
            assert.deepEqual(f.state.reduced, [1]);
            assert.equal(f.state.closes, 1);
        } else {
            assert.deepEqual(f.state.reduced, [1, 2, 3]);
            assert.equal(result.failed, 1);
            assert.equal(result.succeeded, 2);
            assert.equal(result.status, 'failed');
        }
    }
});

test('private receive cursor advances while blocked results remain recoverable on timeout', async () => {
    const f = fixture({tick: 4000});
    const error = await f.run(['a', 'b', 'c'], mapTask, reducer(f), [],
        {ordered: true, timeout: 10}).catch(error => error);
    assert.match(error.cause.message, /TIMEOUT/);
    assert.deepEqual(f.state.calls.map(call => call.timeout), [10, 6, 2]);
    assert.deepEqual(f.state.calls.map(call => call.cursor?.itemIds ?? []), [[], [3], [2, 3]]);
    assert.equal(error.cursor, null);
    assert.equal(error.progress.completed, 0);
    assert.deepEqual(f.state.reduced, []);
    assert.equal(f.state.tasks.length, 3);
    assert.equal(f.state.closes, 1);
});

test('duplicate buffered receipt is rejected rather than reducing twice or polling forever', async () => {
    const f = fixture({choose: () => [3]});
    const error = await f.run(['a', 'b', 'c'], mapTask, reducer(f), [],
        {ordered: true}).catch(error => error);
    assert.match(error.cause.message, /PROTOCOL/);
    assert.equal(f.state.calls.length, 2);
    assert.equal(error.cursor, null);
    assert.equal(f.state.closes, 1);
});

test('ordered is strictly boolean and accessors are not evaluated', async () => {
    for (const ordered of [1, 'true', null, undefined, {}]) {
        const f = fixture();
        await assert.rejects(f.run([], mapTask, reducer(f), [], {ordered}), /INPUT/);
        assert.equal(f.state.factories, 0);
    }
    const f = fixture();
    let called = false;
    const options = Object.defineProperty({}, 'ordered', {get() { called = true; return true; }});
    await assert.rejects(f.run([], mapTask, reducer(f), [], options), /INPUT/);
    assert.equal(called, false);
    assert.equal(f.state.factories, 0);
});

test('ordered empty input keeps the initial value without starting native work', async () => {
    const f = fixture();
    const result = await f.run([], mapTask, reducer(f), false, {ordered: true});
    assert.equal(result.value, false);
    assert.equal(result.status, 'empty');
    assert.deepEqual(f.state.tasks, []);
    assert.equal(f.state.finishes, 0);
    assert.equal(result.cursor, null);
});
