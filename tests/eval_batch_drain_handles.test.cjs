'use strict';
// Exact shipped factory over controlled native replies. This is not a test of
// the Rust drain/waiter, provider calls, session isolation or native QuickJS.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname,'../src/eval/js_kernel/async_host.js'),'utf8');
const plain = value => JSON.parse(JSON.stringify(value));
const hash = 'a'.repeat(64);
const cursor = (seen = [], manifest = hash) => ({schema:'pi.eval.batch-cursor.v1',jobId:'job-a',manifest,seen});
const verdict = key => ({schema:'pi.judgment.v1',answers:{safe:false,confidence:0},provider:'fixture',modelId:key,usage:{input:9,output:3}});
const good = (index, key = index) => ({index,key,ok:true,verdict:verdict(String(key))});
const bad = (index, key = index) => ({index,key,ok:false,error:'explicit item failure'});
function page(overrides = {}) {
    return {schema:'pi.eval.batch-drain.v1',jobId:'job-a',jobStatus:'running',done:false,
        jobFailed:false,complete:false,evidenceAvailable:true,total:2,settled:1,
        items:[good(1)],cursor:cursor([1]),...overrides};
}
const native = details => ({schema:'pi.eval.tool_result.v1',content:[],details,isError:false});
function host() {
    let next = 0;
    let handlers;
    const h = {calls:[],timers:0,command:'eval judgment_batch',drain:async () => page()};
    const context = vm.createContext({tool:{},__pi_set_timer:() => { h.timers++; throw Error('unexpected guest polling'); },
        __pi_clear_timer:() => {},__pi_async_call:(name,payload) => {
            const id = ++next;
            const args = JSON.parse(payload);
            h.calls.push({name,args});
            Promise.resolve().then(async () => {
                if (name === '__pi_tool_result') {
                    assert.equal(args.name,'jobs');
                    return JSON.stringify(native({schema:'pi.host_job.v1',id:'job-a',command:h.command,status:'running',result:null}));
                }
                assert.equal(name,'__pi_judge_batch_drain');
                return h.drain(args);
            }).then(reply => handlers.dispatch(id,false,true,typeof reply === 'string' ? reply : JSON.stringify(reply)),
                error => handlers.dispatch(id,false,false,error.message));
            return id;
        }});
    handlers = vm.runInContext(source,context);
    h.run = code => vm.runInContext(`(async () => { ${code} })()`,context);
    h.attach = async () => { await h.run("globalThis.batch = await judgeBatch.attach('job-a');"); h.calls.length = 0; };
    return h;
}

test('drain uses the native bridge once with seconds converted to milliseconds', async () => {
    const h = host(); await h.attach();
    const result = plain(await h.run('return await batch.drain({timeout:0.025});'));
    assert.deepEqual(result,page());
    assert.deepEqual(h.calls,[{name:'__pi_judge_batch_drain',args:{jobId:'job-a',cursor:null,timeoutMs:25}}]);
    assert.equal(h.timers,0);
});

test('no hidden acknowledgment: processing failure can repeat the same read', async () => {
    const h = host(); await h.attach();
    await h.run("globalThis.bookmark = null; try { const result = await batch.drain({cursor:bookmark}); throw Error('processing failed'); } catch (_) {}");
    const again = plain(await h.run('return await batch.drain({cursor:bookmark});'));
    assert.equal(again.items[0].index,1);
    assert.equal(h.calls.length,2);
    assert.equal(h.calls[0].args.cursor,null);
    assert.equal(h.calls[1].args.cursor,null);
});

test('an earlier unfinished item is returned after a later item was acknowledged', async () => {
    const h = host(); await h.attach();
    await h.run('globalThis.bookmark = (await batch.drain()).cursor;');
    h.drain = async args => {
        assert.deepEqual(args.cursor,cursor([1]));
        return page({jobStatus:'exited',done:true,complete:true,settled:2,items:[good(0)],cursor:cursor([0,1])});
    };
    const result = plain(await h.run('return await batch.drain({cursor:bookmark,timeout:0});'));
    assert.deepEqual(result.items.map(item => item.index),[0]);
});

test('preserves typed keys, false answers, full verdict metadata and explicit item errors', async () => {
    const h = host(); await h.attach();
    const response = page({jobStatus:'failed',done:true,jobFailed:true,complete:true,
        settled:2,items:[good(0,'雪'),bad(1,'__proto__')],cursor:cursor([0,1])});
    h.drain = async () => response;
    assert.deepEqual(plain(await h.run('return await batch.drain();')),response);
});

test('interrupted partial evidence is not promoted to a complete success', async () => {
    const h = host(); await h.attach();
    for (const status of ['killed','timedOut']) {
        const response = page({jobStatus:status,done:true,jobFailed:true});
        h.drain = async () => response;
        const result = plain(await h.run('return await batch.drain();'));
        assert.equal(result.complete,false);
        assert.equal(result.jobFailed,true);
        assert.equal(result.items.length,1);
    }
});

test('privacy withdrawal has no evidence and preserves the supplied cursor', async () => {
    const h = host(); await h.attach();
    await h.run('globalThis.bookmark = (await batch.drain()).cursor;');
    h.drain = async () => page({jobStatus:'failed',done:true,jobFailed:true,evidenceAvailable:false,
        total:null,settled:null,items:[],cursor:cursor([1])});
    const result = plain(await h.run('return await batch.drain({cursor:bookmark});'));
    assert.equal(result.evidenceAvailable,false);
    assert.deepEqual(result.cursor,cursor([1]));
});

test('no initial progress is a valid empty nonblocking observation', async () => {
    const h = host(); await h.attach();
    h.drain = async () => page({evidenceAvailable:false,total:null,settled:null,items:[],cursor:cursor([],null)});
    const result = plain(await h.run('return await batch.drain({timeout:0});'));
    assert.equal(result.done,false);
    assert.equal(result.total,null);
});

test('native timeouts and host refusals propagate without cursor updates or replay', async () => {
    const h = host(); await h.attach();
    await h.run(`globalThis.bookmark = ${JSON.stringify(cursor([1]))};`);
    for (const message of ['EVAL_BATCH_TIMEOUT','current jobs access revoked']) {
        h.drain = async () => { throw Error(message); };
        await assert.rejects(h.run('await batch.drain({cursor:bookmark});'),error => error.message === message);
        assert.deepEqual(plain(await h.run('return bookmark;')),cursor([1]));
    }
    assert.equal(h.calls.length,2);
    assert.equal(h.timers,0);
});

test('completion handles reject drain before a native call', async () => {
    const h = host(); h.command = 'eval model completion';
    await h.run("globalThis.other = await completion.attach('job-a');"); h.calls.length = 0;
    await assert.rejects(h.run('await other.drain();'),/EVAL_BATCH_INPUT/);
    assert.equal(h.calls.length,0);
});

test('invalid options and explicit undefined are rejected before dispatch', async () => {
    const h = host(); await h.attach();
    for (const options of ['null','[]','{retry:1}','{timeout:true}','{timeout:-1}','{timeout:601}',
        '{timeout:Infinity}','{timeout:NaN}','{timeout:undefined}','{cursor:undefined}']) {
        await assert.rejects(h.run(`await batch.drain(${options});`),/EVAL_BATCH_INPUT/);
    }
    assert.equal(h.calls.length,0);
});

test('cursor identity, shape, manifest and duplicate indices are preflighted', async () => {
    const h = host(); await h.attach();
    for (const value of [cursor([1,1]),cursor([-1]),cursor([16]),cursor([true]),cursor([0],null),
        {...cursor(),jobId:'foreign'}, {...cursor(),schema:'wrong'}, {...cursor(),extra:1},
        cursor([],hash.toUpperCase())]) {
        await assert.rejects(h.run(`await batch.drain({cursor:${JSON.stringify(value)}});`),/EVAL_BATCH_INPUT/);
    }
    assert.equal(h.calls.length,0);
});

test('getters and sparse cursor arrays do not run or dispatch', async () => {
    const h = host(); await h.attach();
    await assert.rejects(h.run('globalThis.reads=0; await batch.drain({get cursor(){reads++;return null;}});'),/EVAL_BATCH_INPUT/);
    await assert.rejects(h.run(`const c=${JSON.stringify(cursor())}; c.seen=[,1]; await batch.drain({cursor:c});`),/EVAL_BATCH_INPUT/);
    assert.equal(await h.run('return reads;'),0);
    assert.equal(h.calls.length,0);
});

test('caller mutation after dispatch cannot rewrite the native request', async () => {
    const h = host(); await h.attach();
    h.drain = async args => {
        assert.deepEqual(args.cursor,cursor([1]));
        return page({settled:1,items:[],cursor:cursor([1])});
    };
    await h.run(`const c=${JSON.stringify(cursor([1]))}; const pending=batch.drain({cursor:c}); c.seen.push(0); c.manifest='changed'; await pending;`);
});

test('inherited serializers cannot replace checked job identity or cursor contents', async () => {
    const h = host(); await h.attach();
    h.drain = async args => {
        assert.deepEqual(args,{jobId:'job-a',cursor:cursor([1]),timeoutMs:30000});
        return page({items:[],cursor:cursor([1])});
    };
    await h.run(`const c=${JSON.stringify(cursor([1]))}; Object.prototype.toJSON=()=>({jobId:'foreign'}); Array.prototype.toJSON=()=>[0]; await batch.drain({cursor:c});`);
});

test('a cursor can be reused by a newly attached handle after state reconstruction', async () => {
    const first = host(); await first.attach();
    const bookmark = plain(await first.run('return (await batch.drain()).cursor;'));
    const second = host(); await second.attach();
    second.drain = async () => page({jobStatus:'exited',done:true,complete:true,settled:2,items:[good(0)],cursor:cursor([0,1])});
    const result = plain(await second.run(`return await batch.drain({cursor:${JSON.stringify(bookmark)}});`));
    assert.equal(result.items[0].index,0);
    assert.equal(second.calls.length,1);
});

for (const [name,change] of [
    ['wrong job',p => {p.jobId='foreign';}],
    ['missing cursor',p => {delete p.cursor;}],
    ['null cursor',p => {p.cursor=null;}],
    ['inconsistent done',p => {p.done=true;}],
    ['inconsistent failure',p => {p.jobFailed=true;}],
    ['false completeness',p => {p.complete=true;}],
    ['bad counts',p => {p.settled=2;}],
    ['unseen acknowledgment',p => {p.cursor.seen=[0,1];}],
    ['out of order items',p => {p.settled=2;p.items=[good(1),good(0)];p.cursor.seen=[0,1];}],
    ['mismatched numeric key',p => {p.items[0].key=0;}],
    ['duplicate literal keys',p => {p.settled=2;p.items=[good(0,'same'),good(1,'same')];p.cursor.seen=[0,1];}],
    ['ambiguous result',p => {p.items[0].error='also failed';}],
    ['missing verdict',p => {delete p.items[0].verdict;}],
    ['empty error',p => {p.items=[{index:1,key:1,ok:false,error:''}];}],
    ['withdrawal with evidence',p => {p.evidenceAvailable=false;}],
]) {
    test(`malformed native page: ${name}`,async () => {
        const h=host();await h.attach();const badPage=page();change(badPage);h.drain=async()=>badPage;
        await assert.rejects(h.run('await batch.drain();'),/EVAL_BATCH_PROTOCOL/);
        assert.equal(h.calls.length,1);
    });
}

test('a changed manifest cannot silently begin another sequence',async () => {
    const h=host();await h.attach();h.drain=async()=>page({items:[],cursor:cursor([1],'b'.repeat(64))});
    await assert.rejects(h.run(`await batch.drain({cursor:${JSON.stringify(cursor([1]))}});`),/EVAL_BATCH_PROTOCOL/);
});

test('an acknowledged item cannot be delivered or forgotten in a later reply',async () => {
    const h=host();await h.attach();
    await assert.rejects(h.run(`await batch.drain({cursor:${JSON.stringify(cursor([1]))}});`),/EVAL_BATCH_PROTOCOL/);
    h.drain=async()=>page({settled:0,items:[],cursor:cursor([])});
    await assert.rejects(h.run(`await batch.drain({cursor:${JSON.stringify(cursor([1]))}});`),/EVAL_BATCH_PROTOCOL/);
});

test('malformed encoded replies do not become empty successful pages',async () => {
    const h=host();await h.attach();
    for(const encoded of ['not JSON','null','[]','{}']) {
        h.drain=async()=>encoded;
        await assert.rejects(h.run('await batch.drain();'),/EVAL_BATCH_PROTOCOL/);
    }
});
