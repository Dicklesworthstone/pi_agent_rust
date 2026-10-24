'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/eval/js_kernel/workpool_pipeline.js'), 'utf8');
const plain = value => JSON.parse(JSON.stringify(value));

// Protocol fixture, not a native child or LLM. The production helper is loaded
// verbatim; only the already-existing native pool facade is supplied here.
function fixture(settings = {}) {
    const state = {time: 0, factoryCalls: 0, tasks: [], reduced: 0, highWater: 0,
        events: [], drains: [], closeCalls: 0, finishCalls: 0, published: new Set()};
    let sealed = false;
    const pool = {
        id: null,
        concurrency: settings.concurrency ?? 2,
        async push(tasks) {
            state.events.push('push');
            assert.equal(sealed, false);
            const handles = tasks.map(task => {
                state.tasks.push(task);
                this.id = 'pool-1';
                const itemId = state.tasks.length;
                state.highWater = Math.max(state.highWater, itemId - state.reduced);
                return {itemId, poolId: this.id};
            });
            if (settings.pushError) throw settings.pushError;
            if (settings.badHandle) handles[0].itemId = 32;
            return handles;
        },
        async finish() { state.events.push('finish'); state.finishCalls++; sealed = true; return {id: 'job-1'}; },
        async drain(options) {
            state.events.push('drain');
            state.drains.push(plain(options));
            if (settings.drainError) throw settings.drainError;
            const before = options.cursor?.itemIds ?? [];
            const ready = state.tasks.map((_, index) => index + 1).filter(id => !before.includes(id));
            const chosen = settings.emptyPage ? [] : settings.pageSize
                ? ready.slice(0, Math.min(options.maxItems, settings.pageSize)) : ready.slice(-1);
            const items = chosen.map(id => {
                state.published.add(id);
                const failed = settings.failedId === id;
                return {id, status: failed ? 'failed' : 'completed', result: {
                    content: [{type: 'text', text: state.tasks[id - 1]}],
                    details: {results: [{data: {task: state.tasks[id - 1]}}]}, isError: failed,
                }};
            });
            if (settings.foreignItem && items.length) items[0].id = 31;
            const done = sealed && before.length + chosen.length === state.tasks.length;
            const status = done ? (settings.failedId || settings.carrierFailure ? 'failed' : 'completed') : sealed ? 'draining' : 'open';
            return {schema: 'pi.subagent.pool.drain.v1', poolId: this.id, items,
                cursor: {schema: 'pi.subagent.pool.cursor.v1', poolId: this.id, itemIds: [...before]},
                nextCursor: {schema: 'pi.subagent.pool.cursor.v1', poolId: this.id, itemIds: [...before, ...chosen].sort((a,b) => a-b)},
                done, status, timedOut: items.length === 0 && !done};
        },
        async close() {
            state.events.push('close'); state.closeCalls++;
            if (settings.closeError) throw settings.closeError;
            return {allClosed: !settings.unknownAdmission, unknownAdmission: !!settings.unknownAdmission};
        },
    };
    const factory = async options => { state.factoryCalls++; state.options = options; return pool; };
    const realm = vm.createContext({workpool: factory, Date: {now: () => state.time}});
    vm.runInContext(source, realm);
    return {pool, state, realm, run: factory.mapReduce};
}
const mapper = (input, index) => `task ${index}: ${input}`;
const reduction = fixture => (value, item) => {
    fixture.state.reduced++;
    return [...value, {index: item.index, input: item.input, status: item.status,
        task: item.result.details.results[0].data.task}];
};

test('streams bounded async inputs and reduces out-of-order full native results', async () => {
    const f = fixture();
    let pulled = 0;
    async function* inputs() {
        for (let index = 0; index < 9; index++) {
            assert.ok(pulled - f.state.reduced < 2);
            pulled++;
            await Promise.resolve();
            yield `file-${index}`;
        }
    }
    let published;
    const result = await f.run(inputs(), async (input, index) => mapper(input, index),
        async (value, item) => reduction(f)(value, item), [],
        {pool: {agent: 'reviewer', concurrency: 2}, onPool: pool => { published = pool; }});
    assert.equal(published, f.pool);
    assert.equal(result.pool, f.pool);
    assert.equal(result.completed, 9);
    assert.equal(result.succeeded, 9);
    assert.equal(result.failed, 0);
    assert.equal(f.state.highWater, 2);
    assert.equal(f.state.finishCalls, 1);
    assert.equal(f.state.closeCalls, 0);
    assert.notEqual(result.value[0].index, 0);
    assert.deepEqual(plain(result.value.map(row => row.index).sort((a,b) => a-b)), [0,1,2,3,4,5,6,7,8]);
    assert.deepEqual(plain(result.cursor.itemIds), [1,2,3,4,5,6,7,8,9]);
    assert.deepEqual(f.state.options, {agent: 'reviewer', concurrency: 2});
});

test('empty input creates no native pool, finish request or child', async () => {
    const f = fixture();
    const initial = {count: 0};
    const result = await f.run([], mapper, () => assert.fail('no reduction'), initial);
    assert.equal(result.value, initial);
    assert.equal(result.status, 'empty');
    assert.equal(result.admitted, 0);
    assert.equal(result.cursor, null);
    assert.equal(f.pool.id, null);
    assert.deepEqual(f.state.events, []);
});

test('partial same-page reducer failure retains only successfully reduced item ids', async () => {
    const f = fixture({concurrency: 4, pageSize: 4});
    const cause = new Error('reducer failed');
    const error = await f.run(['a','b','c'], mapper, (value, item) => {
        if (item.id === 2) throw cause;
        f.state.reduced++;
        return value + 1;
    }, 0).catch(error => error);
    assert.equal(error.name, 'WorkpoolPipelineError');
    assert.equal(error.cause, cause);
    assert.equal(error.stage, 'reduce');
    assert.equal(error.pool, f.pool);
    assert.equal(error.progress.value, 1);
    assert.equal(error.progress.completed, 1);
    assert.deepEqual(plain(error.cursor.itemIds), [1]);
    assert.equal(error.item.index, 1);
    assert.deepEqual(plain(error.pending.map(item => item.input)), ['b','c']);
    assert.equal(f.state.closeCalls, 1);
    // Recovery excludes the earlier successful reducer, but retains the failed
    // and not-yet-visited results from the same page.
    const page = await f.pool.drain({cursor: error.cursor, maxItems: 4, timeout: 0});
    assert.deepEqual(page.items.map(item => item.id), [2,3]);
});

test('item failure is explicit and never passed to a success-only reducer', async () => {
    const f = fixture({concurrency: 4, pageSize: 4, failedId: 2});
    const error = await f.run(['a','b'], mapper, reduction(f), []).catch(error => error);
    assert.equal(error.stage, 'result');
    assert.match(error.cause.message, /ITEM_FAILED/);
    assert.equal(error.progress.completed, 1);
    assert.deepEqual(plain(error.cursor.itemIds), [1]);
    assert.equal(error.item.status, 'failed');
    assert.equal(f.state.closeCalls, 1);
});

test('explicit partial-failure policy keeps failed results and truthful counters', async () => {
    const f = fixture({pageSize: 4, failedId: 2});
    const result = await f.run(['a','b','c'], mapper, reduction(f), [], {raiseErrors: false});
    assert.equal(result.completed, 3);
    assert.equal(result.succeeded, 2);
    assert.equal(result.failed, 1);
    assert.equal(result.status, 'failed');
    assert.equal(result.value.find(item => item.index === 1).status, 'failed');
    assert.equal(f.state.closeCalls, 0);
});

test('carrier failure cannot become success just because all items have results', async () => {
    const f = fixture({carrierFailure: true});
    const error = await f.run(['a'], mapper, reduction(f), []).catch(error => error);
    assert.match(error.cause.message, /POOL_FAILED/);
    assert.equal(error.progress.completed, 1);
});

test('exact source capacity succeeds after bounded lookahead', async () => {
    const f = fixture();
    function* inputs() { yield 'a'; yield 'b'; yield 'c'; }
    const result = await f.run(inputs(), mapper, reduction(f), [], {maxItems: 3});
    assert.equal(result.completed, 3);
    assert.equal(f.state.tasks.length, 3);
});

test('overflowing generator is not truncated, mapped beyond limit or replayed', async () => {
    const f = fixture();
    let closed = false;
    let mapped = 0;
    function* inputs() { try { for (let i = 0; i < 8; i++) yield i; } finally { closed = true; } }
    const error = await f.run(inputs(), (value,index) => { mapped++; return mapper(value,index); },
        reduction(f), [], {maxItems: 3}).catch(error => error);
    assert.match(error.cause.message, /CAPACITY/);
    assert.equal(mapped, 3);
    assert.equal(f.state.tasks.length, 3);
    assert.equal(closed, true);
    assert.equal(f.state.closeCalls, 1);
});

test('ambiguous admission is attempted once and exposes native cleanup uncertainty', async () => {
    const cause = new Error('reply lost after admission');
    const f = fixture({pushError: cause, unknownAdmission: true});
    const error = await f.run(['a'], mapper, reduction(f), []).catch(error => error);
    assert.equal(error.cause, cause);
    assert.equal(error.stage, 'admission');
    assert.equal(f.state.tasks.length, 1);
    assert.equal(error.progress.admitted, 0);
    assert.equal(error.cleanup.allClosed, false);
    assert.equal(error.cleanup.unknownAdmission, true);
    assert.equal(f.state.finishCalls, 0);
});

test('lost drain reply never retries admission or advances the cursor', async () => {
    const cause = new Error('drain unavailable');
    const f = fixture({drainError: cause});
    const error = await f.run(['a','b'], mapper, reduction(f), []).catch(error => error);
    assert.equal(error.stage, 'drain');
    assert.equal(error.cause, cause);
    assert.equal(error.cursor, null);
    assert.equal(f.state.tasks.length, 2);
    assert.equal(f.state.closeCalls, 1);
});

test('one shared deadline shrinks between result pages and keeps completed reductions', async () => {
    const f = fixture({concurrency: 1});
    const error = await f.run(['a','b','c'], mapper, (value, item) => {
        f.state.time += 6000;
        f.state.reduced++;
        return value + 1;
    }, 0, {timeout: 10}).catch(error => error);
    assert.match(error.cause.message, /TIMEOUT/);
    assert.deepEqual(f.state.drains.map(call => call.timeout), [10,4]);
    assert.equal(error.progress.completed, 2);
    assert.deepEqual(plain(error.cursor.itemIds), [1,2]);
    assert.equal(f.state.tasks.length, 2);
});

test('empty timed-out native page does not enter a guest polling loop', async () => {
    const f = fixture({emptyPage: true});
    const error = await f.run(['a'], mapper, reduction(f), []).catch(error => error);
    assert.match(error.cause.message, /TIMEOUT/);
    assert.equal(f.state.drains.length, 1);
});

test('pool closes before source cleanup, with secondary failures retained', async () => {
    const closeError = new Error('native close unavailable');
    const returnError = new Error('iterator close unavailable');
    const primary = new Error('mapping failed');
    const f = fixture({closeError});
    const source = {
        [Symbol.iterator]() { return this; },
        next() { return {done: false, value: 'a'}; },
        return() { f.state.events.push('return'); throw returnError; },
    };
    const error = await f.run(source, () => { throw primary; }, reduction(f), []).catch(error => error);
    assert.equal(error.cause, primary);
    assert.deepEqual(f.state.events, ['close','return']);
    assert.equal(error.cleanupErrors[0], closeError);
    assert.equal(error.cleanupErrors[1], returnError);
});

test('async source cleanup is awaited after callback failure', async () => {
    const f = fixture();
    let closed = false;
    async function* inputs() {
        try { yield 'a'; yield 'b'; }
        finally { await Promise.resolve(); closed = true; }
    }
    await assert.rejects(f.run(inputs(), () => null, reduction(f), []));
    assert.equal(closed, true);
    assert.equal(f.state.tasks.length, 0);
});

test('source failure does not replace its cause or replay pending work', async () => {
    const f = fixture();
    const cause = new Error('input read failed');
    function* inputs() { yield 'a'; throw cause; }
    const error = await f.run(inputs(), mapper, reduction(f), []).catch(error => error);
    assert.equal(error.cause, cause);
    assert.equal(error.stage, 'source');
    assert.equal(f.state.tasks.length, 1);
});

test('invalid acknowledgments and foreign native items fail closed', async () => {
    for (const settings of [{badHandle: true}, {foreignItem: true}]) {
        const f = fixture(settings);
        const error = await f.run(['a'], mapper, reduction(f), []).catch(error => error);
        assert.match(error.cause.message, /PROTOCOL/);
        assert.equal(f.state.reduced, 0);
        assert.equal(f.state.closeCalls, 1);
    }
});

test('input validation rejects unknown options, booleans, overflow and accessors before creation', async () => {
    for (const options of [
        {maxItems: 0}, {maxItems: 33}, {maxItems: true}, {timeout: NaN},
        {timeout: Infinity}, {timeout: -1}, {timeout: 0}, {timeout: 601},
        {raiseErrors: 1}, {onPool: 7}, {misspelled: true},
    ]) {
        const f = fixture();
        await assert.rejects(f.run([], mapper, reduction(f), [], options), /INPUT/);
        assert.equal(f.state.factoryCalls, 0);
    }
    const f = fixture();
    let invoked = false;
    const options = Object.defineProperty({}, 'timeout', {get() { invoked = true; return 30; }});
    await assert.rejects(f.run([], mapper, reduction(f), [], options), /INPUT/);
    assert.equal(invoked, false);
    await assert.rejects(f.run(['a','b'], mapper, reduction(f), [], {maxItems: 1}), /INPUT/);
    assert.equal(f.state.factoryCalls, 0);
});

test('captured factory survives replacement of the guest global', async () => {
    const f = fixture();
    f.realm.workpool = () => assert.fail('replacement must not acquire authority');
    const result = await f.run(['a'], mapper, reduction(f), []);
    assert.equal(result.completed, 1);
    assert.equal(f.state.factoryCalls, 1);
});

test('reducer accepts false, null and zero without treating them as missing', async () => {
    for (const expected of [false, null, 0]) {
        const f = fixture();
        const result = await f.run(['a'], mapper, () => { f.state.reduced++; return expected; }, 'initial');
        assert.equal(result.value, expected);
        assert.equal(result.completed, 1);
    }
});
