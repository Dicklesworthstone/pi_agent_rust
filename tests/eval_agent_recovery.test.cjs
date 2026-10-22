'use strict';
// Executes the complete production guest factory. The controlled transport is
// not evidence of Rust, process-lifecycle or live-model acceptance.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname,'../src/eval/js_kernel/async_host.js'),'utf8');
const plain = value => JSON.parse(JSON.stringify(value));
const output = (text, isError = false) => ({content:[{type:'text',text}],details:{answer:text},isError});
const wrapper = details => ({schema:'pi.eval.tool_result.v1',content:[],details,isError:false});
const worker = (turnId = 3, status = 'completed', extra = {}) => ({
    schema:'pi.subagent.worker.v1',workerId:'worker-a',jobId:'job-a',agent:'reviewer',
    status:status === 'running' ? 'running' : 'idle',turnId,
    turn:{id:turnId,status,result:status === 'queued' || status === 'running' ? null : output(`turn-${turnId}`)},
    ...extra,
});
const pool = (states = ['completed','running'], status = 'open', extra = {}) => ({
    schema:'pi.subagent.pool.v1',poolId:'pool-a',jobId:'job-pool',agent:'reviewer',status,
    sealed:status !== 'open',concurrency:2,remainingMs:12000,admittedItems:states.length,
    queuedItems:states.filter(s => s === 'queued').length,
    runningItems:states.filter(s => s === 'running').length,
    completedItems:states.filter(s => s === 'completed').length,
    failedItems:states.filter(s => s === 'failed' || s === 'cancelled').length,
    items:states.map((status,index) => ({id:index+1,status,summary:`item-${index+1}`,
        resultRef:{poolId:'pool-a',itemId:index+1}})),...extra,
});
function host(respond) {
    let next = 1;
    let handlers;
    const h = {calls:[],respond};
    h.context = vm.createContext({tool:{},__pi_set_timer:() => {throw new Error('unexpected timer');},
        __pi_clear_timer:() => {},__pi_async_call:(name,encoded) => {
            const id = next++;
            const wire = JSON.parse(encoded);
            assert.equal(name,'__pi_tool_result');
            h.calls.push(wire);
            Promise.resolve().then(() => h.respond(wire.name,wire.args)).then(
                details => handlers.dispatch(id,false,true,JSON.stringify(wrapper(details))),
                error => handlers.dispatch(id,false,false,error.message),
            );
            return id;
        }});
    handlers = vm.runInContext(source,h.context);
    h.run = code => vm.runInContext(`(async () => { ${code} })()`,h.context);
    h.clear = () => handlers.clear();
    h.flush = () => new Promise(resolve => setImmediate(resolve));
    return h;
}

test('attach worker observes latest turn without starting a child or sending a prompt', async () => {
    const h = host(async () => worker());
    const value = plain(await h.run("globalThis.w = await agent.attachWorker('worker-a'); return {id:w.id,jobId:w.jobId,agent:w.agent,turn:w.turnId,recovered:w.recovered,sendAllowed:w.sendAllowed,unknown:w.unknownAdmission};"));
    assert.deepEqual(value,{id:'worker-a',jobId:'job-a',agent:'reviewer',turn:3,recovered:true,sendAllowed:false,unknown:false});
    assert.deepEqual(h.calls,[{name:'subagent',args:{operation:'worker_status',workerId:'worker-a'}}]);
    await assert.rejects(h.run("await w.send('must not dispatch');"),/RECOVERY_REQUIRED/);
    assert.equal(h.calls.length,1);
});

test('worker recovery can pin an earlier turn and retrieve complete evidence', async () => {
    const h = host(async (_,args) => worker(args.turnId ?? 3));
    const value = plain(await h.run("globalThis.w = await agent.attachWorker('worker-a',{turnId:1}); globalThis.t = await w.turn(2); return [w.turnId,t.turnId,(await t.output()).details.answer];"));
    assert.deepEqual(value,[1,2,'turn-2']);
    assert.deepEqual(h.calls.map(call => call.args.turnId),[1,2,2]);
});

test('explicit send permission queues only the new prompt on the recovered worker', async () => {
    const h = host(async (_,args) => args.operation === 'worker_send' ? worker(4,'queued') : worker());
    const value = plain(await h.run("const w = await agent.attachWorker('worker-a',{allowSend:true}); const t = await w.send('new work only'); return [w.turnId,t.turnId,w.jobId];"));
    assert.deepEqual(value,[4,4,'job-a']);
    assert.deepEqual(h.calls[1],{name:'subagent',args:{operation:'worker_send',workerId:'worker-a',task:'new work only'}});
    assert.equal(h.calls.length,2);
});

test('attach rejects invalid identifiers and control overrides before any host call', async () => {
    const h = host(async () => {throw new Error('must not call');});
    for (const expr of ["agent.attachWorker('')", "agent.attachWorker('a b')", "agent.attachWorker('x'.repeat(129))",
        "agent.attachWorker(1)","agent.attachWorker('worker-a',null)","agent.attachWorker('worker-a',{allowSend:1})",
        "agent.attachWorker('worker-a',{turnId:0})","agent.attachWorker('worker-a',{turnId:33})",
        "agent.attachWorker('worker-a',{turnId:1.5})","agent.attachWorker('worker-a',{model:'other'})",
        "workpool.attach('pool-a',{allowPush:'true'})","workpool.attach('pool-a',{timeoutSeconds:300})",
        "workpool.attach('pool-a',{turnId:1})","workpool.attach('pool-a',[])"]) {
        await assert.rejects(h.run(`return ${expr};`),/INPUT/);
    }
    assert.equal(h.calls.length,0);
});

test('worker identity and pinned-turn mismatches refuse attachment', async () => {
    for (const snapshot of [null,{},worker(3,'completed',{workerId:'worker-other'}),worker(3,'completed',{jobId:''}),
        worker(3,'completed',{agent:''}),worker(3,'completed',{schema:'wrong'}),worker(1,'completed',{turn:{id:2,status:'completed',result:output('x')}})]) {
        const h = host(async () => snapshot);
        await assert.rejects(h.run("return agent.attachWorker('worker-a',{turnId:1});"),/PROTOCOL/);
        assert.equal(h.calls.length,1);
    }
});

test('every recovered operation re-enters current policy rather than caching output', async () => {
    const h = host(async () => worker());
    await h.run("globalThis.w = await agent.attachWorker('worker-a');");
    h.respond = async () => {throw new Error('current host denied');};
    for (const call of ['w.status()','w.output()','w.wait(0)','w.turn(1)','w.close()']) {
        await assert.rejects(h.run(`return ${call};`),/current host denied/);
    }
    assert.equal(h.calls.length,6);
});

test('lost send acknowledgement disables further sends but preserves known turns', async () => {
    const h = host(async (_,args) => {
        if (args.operation === 'worker_send') throw new Error('reply lost after admission');
        return worker(args.turnId ?? 3);
    });
    await h.run("globalThis.w = await agent.attachWorker('worker-a',{allowSend:true});");
    await assert.rejects(h.run("return w.send('maybe admitted');"),/reply lost/);
    assert.deepEqual(plain(await h.run('return [w.unknownAdmission,w.sendAllowed,w.turnId];')),[true,false,3]);
    await assert.rejects(h.run("return w.send('do not replay');"),/RECOVERY_REQUIRED/);
    assert.equal(h.calls.filter(c => c.args.operation === 'worker_send').length,1);
    assert.equal((await h.run('return (await w.output()).details.answer;')),'turn-3');
});

test('malformed send acknowledgment also closes guest admission without retry', async () => {
    const h = host(async (_,args) => args.operation === 'worker_send' ? worker(4,'queued',{jobId:'wrong'}) : worker());
    await h.run("globalThis.w = await agent.attachWorker('worker-a',{allowSend:true});");
    await assert.rejects(h.run("return w.send('maybe');"),/PROTOCOL/);
    assert.equal(await h.run('return w.unknownAdmission;'),true);
    await assert.rejects(h.run("return w.send('again');"),/RECOVERY_REQUIRED/);
    assert.equal(h.calls.length,2);
});

test('clearing an active cell marks its unacknowledged send uncertain in the next cell', async () => {
    let release;
    const h = host(async (_,args) => args.operation === 'worker_send'
        ? new Promise(resolve => {release = resolve;}) : worker());
    await h.run("globalThis.w = await agent.attachWorker('worker-a',{allowSend:true});");
    void h.run("await w.send('accepted but interrupted');");
    await h.flush();
    h.clear();
    assert.deepEqual(plain(await h.run('return [w.sendAllowed,w.unknownAdmission];')),[false,true]);
    await assert.rejects(h.run("return w.send('must not repeat');"),/RECOVERY_REQUIRED/);
    release(worker(4,'queued'));
    await h.flush();
    assert.equal(await h.run('return w.turnId;'),3);
    assert.equal(h.calls.filter(c => c.args.operation === 'worker_send').length,1);
});

test('explicit reattachment reconciles an unknown send without reopening the old handle', async () => {
    let lost = true;
    const h = host(async (_,args) => {
        if (args.operation === 'worker_send' && lost) {lost = false;throw new Error('lost');}
        if (args.operation === 'worker_send') return worker(5,'queued');
        return worker(lost ? 3 : 4);
    });
    await h.run("globalThis.old = await agent.attachWorker('worker-a',{allowSend:true});");
    await assert.rejects(h.run("return old.send('first');"),/lost/);
    await h.run("globalThis.fresh = await agent.attachWorker('worker-a',{allowSend:true}); await fresh.send('different follow-up');");
    assert.deepEqual(plain(await h.run('return [old.sendAllowed,old.turnId,fresh.turnId];')),[false,3,5]);
    assert.deepEqual(h.calls.filter(c => c.args.operation === 'worker_send').map(c => c.args.task),['first','different follow-up']);
});

test('concurrent acknowledged sends retain out-of-order turn handles and newest selection', async () => {
    const waiting = [];
    const h = host(async (_,args) => args.operation === 'worker_send'
        ? new Promise(resolve => waiting.push(resolve)) : worker(1));
    await h.run("globalThis.w = await agent.attachWorker('worker-a',{allowSend:true});");
    const first = h.run("return w.send('a');");
    const second = h.run("return w.send('b');");
    await h.flush();
    waiting[1](worker(3,'queued'));
    assert.equal((await second).turnId,3);
    waiting[0](worker(2,'queued'));
    assert.equal((await first).turnId,2);
    assert.deepEqual(plain(await h.run('return [w.turnId,w.sendAllowed,w.unknownAdmission];')),[3,true,false]);
});

test('a successful concurrent send cannot erase another send uncertainty', async () => {
    let release;
    const h = host(async (_,args) => {
        if (args.operation !== 'worker_send') return worker(1);
        if (args.task === 'lost') throw new Error('lost reply');
        return new Promise(resolve => {release = resolve;});
    });
    await h.run("globalThis.w = await agent.attachWorker('worker-a',{allowSend:true});");
    const success = h.run("return w.send('accepted');");
    await assert.rejects(h.run("return w.send('lost');"),/lost reply/);
    release(worker(2,'queued'));
    assert.equal((await success).turnId,2);
    assert.deepEqual(plain(await h.run('return [w.turnId,w.sendAllowed,w.unknownAdmission];')),[2,false,true]);
});

test('newly launched workers also fence a failed send without changing normal launch', async () => {
    const h = host(async (_,args) => {
        if (args.operation === 'worker_start') return worker(1,'queued',{agent:'task'});
        throw new Error('lost');
    });
    await h.run("globalThis.w = await agent('seed',{keepAlive:true});");
    assert.deepEqual(plain(await h.run('return [w.recovered,w.sendAllowed];')),[false,true]);
    await assert.rejects(h.run("return w.send('maybe');"),/lost/);
    await assert.rejects(h.run("return w.send('replay');"),/RECOVERY_REQUIRED/);
    assert.equal(h.calls.length,2);
});

test('attach pool reconstructs the complete ordered manifest without starting or sealing it', async () => {
    const h = host(async () => pool());
    const result = plain(await h.run("globalThis.p = await workpool.attach('pool-a'); return {id:p.id,job:p.jobId,closed:p.closed,recovered:p.recovered,items:p.items.map(i=>[i.itemId,i.poolId,i.jobId])};"));
    assert.deepEqual(result,{id:'pool-a',job:'job-pool',closed:true,recovered:true,items:[[1,'pool-a','job-pool'],[2,'pool-a','job-pool']]});
    assert.deepEqual(h.calls,[{name:'subagent',args:{operation:'pool_status',poolId:'pool-a'}}]);
    await assert.rejects(h.run("return p.push(['do not restart']);"),/POOL_CLOSED/);
    assert.equal(h.calls.length,1);
});

test('recovered pool items use the existing typed result and mixed wait path', async () => {
    const h = host(async (_,args) => args.itemId === undefined ? pool(['completed','completed'])
        : pool(['completed','completed'],'open',{itemId:args.itemId,item:{id:args.itemId,status:'completed',result:output(`item-${args.itemId}`)}}));
    const results = plain(await h.run("const p = await workpool.attach('pool-a'); return await wait([p.items[1],p.items[0]],{timeout:0});"));
    assert.deepEqual(results.map(r => r.details.answer),['item-2','item-1']);
    assert.deepEqual(h.calls.slice(1).map(c => [c.args.operation,c.args.itemId,c.args.timeoutMs]),[['pool_wait',2,0],['pool_wait',1,0]]);
});

test('explicit resumed push uses only the existing pool id and new tasks', async () => {
    const h = host(async (_,args) => args.operation === 'pool_push'
        ? pool(['completed','running','queued'],'open',{admittedItemIds:[3]}) : pool());
    const result = plain(await h.run("const p = await workpool.attach('pool-a',{allowPush:true}); const added = await p.push(['new review']); return [p.items.length,added[0].itemId];"));
    assert.deepEqual(result,[3,3]);
    assert.deepEqual(h.calls[1],{name:'subagent',args:{operation:'pool_push',poolId:'pool-a',tasks:['new review']}});
    assert.equal(h.calls.some(c => c.args.operation === 'pool_start'),false);
});

test('reattaching sealed pools returns aggregate handles without resending finish', async () => {
    for (const status of ['draining','completed','failed','cancelled']) {
        const h = host(async () => pool(['completed','completed'],status));
        const result = plain(await h.run("const p = await workpool.attach('pool-a',{allowPush:true}); const job = await p.finish(); return [p.closed,job.id];"));
        assert.deepEqual(result,[true,'job-pool']);
        assert.equal(h.calls.length,1);
    }
});

test('inspection-only recovered pools may explicitly finish or cancel', async () => {
    const h = host(async (_,args) => args.operation === 'pool_finish'
        ? pool(['completed','running'],'draining') : args.operation === 'pool_close'
        ? pool(['completed','cancelled'],'cancelled') : pool());
    await h.run("globalThis.p = await workpool.attach('pool-a'); await p.finish(); await p.close();");
    assert.deepEqual(h.calls.map(c => c.args.operation),['pool_status','pool_finish','pool_close']);
});

test('pool identity, counts, references and result mismatches fail before returning a handle', async () => {
    for (const snapshot of [null,{},pool([], 'open',{poolId:'other'}),pool([], 'open',{jobId:''}),
        pool([], 'open',{agent:'x'.repeat(129)}),pool(['completed'],'open',{completedItems:0}),
        pool(['completed'],'open',{items:[{id:1,status:'completed',summary:'x',resultRef:{poolId:'other',itemId:1}}]})]) {
        const h = host(async () => snapshot);
        await assert.rejects(h.run("return workpool.attach('pool-a');"),/PROTOCOL/);
        assert.equal(h.calls.length,1);
    }
});

test('a lost recovered pool push stays closed and can reconcile admitted items', async () => {
    let lost = false;
    const h = host(async (_,args) => {
        if (args.operation === 'pool_push') {lost = true;throw new Error('lost pool acknowledgement');}
        return pool(lost ? ['completed','running','queued'] : ['completed','running']);
    });
    await h.run("globalThis.p = await workpool.attach('pool-a',{allowPush:true});");
    await assert.rejects(h.run("return p.push(['maybe admitted']);"),/POOL_ADMISSION/);
    await h.run('await p.peek();');
    assert.deepEqual(plain(await h.run('return [p.items.length,p.closed,p.unknownAdmission];')),[3,true,false]);
    await assert.rejects(h.run("return p.push(['must not repeat']);"),/POOL_CLOSED/);
    assert.equal(h.calls.filter(c => c.args.operation === 'pool_push').length,1);
});

test('another kernel can reconstruct handles using only native ids', async () => {
    const calls = [];
    const respond = async (name,args) => {calls.push({name,args});return args.operation === 'worker_status' ? worker() : pool();};
    const old = host(respond);
    const ids = plain(await old.run("const w = await agent.attachWorker('worker-a'); const p = await workpool.attach('pool-a'); return [w.id,p.id];"));
    old.clear();
    const fresh = host(respond);
    const result = plain(await fresh.run(`const w = await agent.attachWorker(${JSON.stringify(ids[0])}); const p = await workpool.attach(${JSON.stringify(ids[1])}); return [w.turnId,p.items.length];`));
    assert.deepEqual(result,[3,2]);
    assert.equal(calls.every(c => c.args.operation.endsWith('_status')),true);
});

test('missing or foreign native identities fail and never fall back to launch', async () => {
    const h = host(async () => {throw new Error('native identity unavailable in current session');});
    for (const call of ["agent.attachWorker('worker-missing')","workpool.attach('pool-foreign')"]) {
        await assert.rejects(h.run(`return ${call};`),/unavailable/);
    }
    assert.deepEqual(h.calls.map(c=>c.args.operation),['worker_status','pool_status']);
});

test('invalid prompts and turn indices do not close a healthy worker or dispatch', async () => {
    const h = host(async () => worker());
    await h.run("globalThis.w = await agent.attachWorker('worker-a',{allowSend:true});");
    for (const call of ['w.send(null)','w.send(42)',"w.send('   ')",'w.turn(0)','w.turn(33)','w.turn(true)']) {
        await assert.rejects(h.run(`return ${call};`),/INPUT/);
    }
    assert.deepEqual(plain(await h.run('return [w.sendAllowed,w.unknownAdmission];')),[true,false]);
    assert.equal(h.calls.length,1);
});

test('worker turn handles recovered from a closed worker remain fixed and failures stay failures', async () => {
    const h = host(async (_,args) => worker(args.turnId ?? 3,'failed',{status:'closed',
        turn:{id:args.turnId ?? 3,status:'failed',result:output('cancelled turn',true)}}));
    const result = plain(await h.run("const w = await agent.attachWorker('worker-a'); const t = await w.turn(1); return await wait([t],{timeout:0,raiseErrors:false});"));
    assert.equal(result[0].isError,true);
    await assert.rejects(h.run("const w = await agent.attachWorker('worker-a'); return w.wait(0);"),/cancelled turn/);
});

test('recovery captures explicit permissions before waiting for a native snapshot', async () => {
    let release;
    const h = host(async () => new Promise(resolve => {release = resolve;}));
    const workerPending = h.run("globalThis.opts = {allowSend:false}; return agent.attachWorker('worker-a',opts);");
    await h.flush();
    await h.run('opts.allowSend = true;');
    release(worker());
    assert.equal((await workerPending).sendAllowed,false);
    const poolPending = h.run("globalThis.opts = {allowPush:false}; return workpool.attach('pool-a',opts);");
    await h.flush();
    await h.run('opts.allowPush = true;');
    release(pool());
    assert.equal((await poolPending).closed,true);
});

test('inherited permission flags do not enable recovered submissions', async () => {
    const h = host(async (_,args) => args.operation === 'worker_status' ? worker() : pool());
    const result = plain(await h.run("const w = await agent.attachWorker('worker-a',Object.create({allowSend:true})); const p = await workpool.attach('pool-a',Object.create({allowPush:true})); return [w.sendAllowed,p.closed];"));
    assert.deepEqual(result,[false,true]);
});

test('recovered pool retains context observations and explicit incremental result acknowledgments', async () => {
    let completed = false;
    const cursor = itemIds => ({schema:'pi.subagent.pool.cursor.v1',poolId:'pool-a',itemIds});
    const h = host(async (_,args) => {
        if (args.operation === 'pool_status') return pool(['running','completed'],'open',{
            workerContexts:[{workerIndex:1,tokens:400,window:8000},{workerIndex:2,tokens:100,window:8000}],
        });
        assert.equal(args.operation,'pool_drain');
        const before = args.cursor?.itemIds ?? [];
        const settled = completed ? [1,2] : [2];
        const items = settled.filter(id => !before.includes(id)).map(id => ({id,status:'completed',result:output(`item-${id}`)}));
        return {schema:'pi.subagent.pool.drain.v1',poolId:'pool-a',jobId:'job-pool',agent:'reviewer',
            status:completed ? 'completed' : 'open',sealed:completed,admittedItems:2,
            settledItems:settled.length,pendingItems:2-settled.length,items,
            cursor:cursor(before),nextCursor:cursor(settled),hasMore:false,
            done:completed,timedOut:items.length === 0 && !completed};
    });
    await h.run("globalThis.p = await workpool.attach('pool-a');");
    assert.equal(await h.run('return p.closed;'),true);
    assert.deepEqual(plain(await h.run('return (await p.peek()).workerContexts;')),
        [{workerIndex:1,tokens:400,window:8000},{workerIndex:2,tokens:100,window:8000}]);
    await h.run('globalThis.first = await p.drain({timeout:0});');
    assert.deepEqual(plain(await h.run('return first.items.map(item => item.id);')),[2]);
    assert.deepEqual(plain(await h.run('return (await p.drain({timeout:0})).items.map(item => item.id);')),[2]);
    assert.equal(await h.run('return (await p.drain({cursor:first.nextCursor,timeout:0})).timedOut;'),true);
    completed = true;
    assert.deepEqual(plain(await h.run('const page = await p.drain({cursor:first.nextCursor,timeout:0}); return [page.items.map(item => item.id),page.nextCursor.itemIds,page.done];')),
        [[1],[1,2],true]);
    assert.ok(h.calls.every(call => call.args.operation === 'pool_status' || call.args.operation === 'pool_drain'));
});
