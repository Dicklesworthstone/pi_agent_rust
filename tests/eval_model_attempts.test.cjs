'use strict';
// Shipped helper with controlled native replies. This verifies guest protocol
// and recovery semantics, not the Rust job owner, providers or native QuickJS.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../src/eval/js_kernel/async_host.js'), 'utf8');
const plain = value => JSON.parse(JSON.stringify(value));
const receipt = (attempt, status, operation = 'completion') => ({
    attempt, operation, status, provider:'fixture', modelId:`model-${attempt}`,
    model:`fixture/model-${attempt}`, api:'controlled', thinkingLevel:null,
    usage:null, usageKnown:false,
    ...(status === 'failed' ? {error:'model request failed', fallbackScheduled:true} : {}),
});
const progress = (attempts, operation = 'completion', interrupted = false) => ({
    content:[], isError:interrupted, details:{
        schema:interrupted ? 'pi.host_job.interrupted.v1' : 'pi.host_job.progress.v1',
        partialResult:{content:[], isError:false, details:{
            schema:'pi.eval.model-job.attempts.v1', operation, complete:false,
            judgments:{schema:'pi.eval.judgments.v1', attempts},
        }},
    },
});
const settled = (attempts, operation = 'completion', failed = false) => ({
    content:[{type:'text', text:failed ? 'model request failed' : 'ready'}], isError:failed,
    details:{schema:'pi.eval.model-job.result.v1', operation,
        result:failed ? null : operation === 'completion'
            ? {schema:'pi.completion.v1', text:'ready'}
            : {schema:'pi.judgment.v1', answers:{safe:false, score:0}},
        judgments:{schema:'pi.eval.judgments.v1', attempts},
        ...(failed ? {error:'model request failed'} : {}),
    },
});

async function host(operation = 'completion') {
    let next = 0;
    let handlers;
    const h = {calls:[], failure:null, snapshot:{schema:'pi.host_job.v1', id:'job-a',
        command:operation === 'completion' ? 'eval model completion' : 'eval judgment',
        status:'running', result:null}};
    const context = vm.createContext({tool:{}, __pi_set_timer:() => { throw Error('unexpected polling'); },
        __pi_clear_timer:() => {}, __pi_async_call:(name, payload) => {
            const id = ++next;
            const args = JSON.parse(payload);
            h.calls.push({name, args});
            Promise.resolve().then(() => {
                assert.equal(name, '__pi_tool_result');
                assert.equal(args.name, 'jobs');
                return {schema:'pi.eval.tool_result.v1',
                    content:h.failure ? [{type:'text', text:h.failure}] : [],
                    details:h.failure ? null : h.snapshot, isError:!!h.failure};
            }).then(value => handlers.dispatch(id, false, true, JSON.stringify(value)),
                error => handlers.dispatch(id, false, false, error.message));
            return id;
        }});
    handlers = vm.runInContext(source, context);
    h.run = code => vm.runInContext(`(async () => { ${code} })()`, context);
    await h.run(`globalThis.job = await ${operation === 'completion' ? 'completion' : 'judge'}.attach('job-a');`);
    h.calls.length = 0;
    return h;
}

test('no receipt yet returns an empty observation through one current-host call', async () => {
    const h = await host();
    assert.deepEqual(plain(await h.run('return await job.attempts();')), []);
    assert.equal(h.calls.length, 1);
    assert.deepEqual(h.calls[0].args, {name:'jobs', args:{action:'wait', jobId:'job-a', timeoutMs:0}});
});

test('running receipts preserve order, binding provenance and unknown usage', async () => {
    const h = await host();
    const attempts = [receipt(5, 'failed'), receipt(6, 'running')];
    h.snapshot.result = progress(attempts);
    assert.deepEqual(plain(await h.run('return await job.attempts();')), attempts);
    assert.equal(h.calls.length, 1);
});

test('settled completion and judgment preserve values and attempt history', async () => {
    for (const operation of ['completion', 'judgment']) {
        const h = await host(operation);
        const attempts = [receipt(1, 'failed', operation), receipt(2, 'completed', operation)];
        h.snapshot.status = 'exited';
        h.snapshot.result = settled(attempts, operation);
        assert.deepEqual(plain(await h.run('return await job.attempts();')), attempts);
        assert.deepEqual(plain(await h.run('return await job.wait();')),
            operation === 'completion' ? 'ready' : {safe:false, score:0});
    }
});

test('failed terminal receipts remain readable without changing wait failure', async () => {
    const h = await host();
    const attempts = [receipt(1, 'failed')];
    h.snapshot.status = 'failed';
    h.snapshot.result = settled(attempts, 'completion', true);
    assert.deepEqual(plain(await h.run('return await job.attempts();')), attempts);
    await assert.rejects(h.run('await job.wait();'), /model request failed/);
});

test('interrupted attempts retain partial evidence without fabricating completion', async () => {
    const h = await host();
    for (const status of ['killed', 'timedOut']) {
        const attempts = [receipt(1, 'running')];
        h.snapshot.status = status;
        h.snapshot.result = progress(attempts, 'completion', true);
        assert.deepEqual(plain(await h.run('return await job.attempts();')), attempts);
        await assert.rejects(h.run('await job.wait();'), new RegExp(status));
    }
});

test('an observation rechecks host policy instead of serving cached receipts', async () => {
    const h = await host();
    h.snapshot.result = progress([receipt(1, 'running')]);
    await h.run('await job.attempts();');
    h.failure = 'current jobs access revoked';
    await assert.rejects(h.run('await job.attempts();'), /current jobs access revoked/);
    assert.equal(h.calls.length, 2);
});

test('malformed running receipts are rejected before becoming observations', async () => {
    const h = await host();
    const invalid = [[], [null], [receipt(true, 'running')], [receipt(1, 'queued')],
        [receipt(1, 'failed'), receipt(3, 'running')], [receipt(1, 'running', 'judgment')],
        [{...receipt(1, 'running'), usageKnown:'false'}],
        Array.from({length:17}, (_, i) => receipt(i + 1, 'running'))];
    for (const attempts of invalid) {
        h.snapshot.result = progress(attempts);
        await assert.rejects(h.run('await job.attempts();'), /EVAL_MODEL_PROTOCOL/);
    }
});

test('malformed terminal attempt collections and foreign jobs are rejected', async () => {
    const h = await host();
    h.snapshot.status = 'exited';
    for (const attempts of [[null], {}, Array.from({length:17}, (_, i) => receipt(i + 1, 'completed'))]) {
        h.snapshot.result = settled(attempts);
        await assert.rejects(h.run('await job.attempts();'), /EVAL_MODEL_PROTOCOL/);
    }
    h.snapshot.result = settled([receipt(1, 'completed')]);
    h.snapshot.id = 'foreign-job';
    await assert.rejects(h.run('await job.attempts();'), /EVAL_AGENT_PROTOCOL/);
});
