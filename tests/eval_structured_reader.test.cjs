'use strict';
// Diagnostics for the actual embedded JS factory, not a native/DSR substitute.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../src/eval/js_kernel/async_host.js'), 'utf8');
function host() {
    let next = 1;
    const requests = [];
    const context = vm.createContext({tool:{},
        __pi_set_timer: () => next++, __pi_clear_timer: () => {},
        __pi_async_call: (name, payload) => {
            const id = next++; requests.push({id, name, args:JSON.parse(payload)}); return id;
        }});
    const dispatch = vm.runInContext(source, context);
    return {context, requests, dispatch};
}
const envelope = details => ({schema:'pi.eval.tool_result.v1', content:[{type:'text',text:'payload'}], details, isError:false});

test('text calls retain their wire shape and return a string', async () => {
    const h = host(); const result = vm.runInContext("tool.call('read', {path:'a'})", h.context);
    assert.deepEqual(h.requests[0], {id:1,name:'read',args:{path:'a'}});
    h.dispatch.dispatch(1,false,true,'text-only');
    assert.equal(await result,'text-only');
});
test('structured calls use an explicit operation and return arbitrary details', async () => {
    for (const details of [null,42,[1,'x'],{nested:{value:true}}]) {
        const h = host(); const result = vm.runInContext("tool.result('read', {path:'a'})", h.context);
        assert.deepEqual(h.requests[0], {id:1,name:'__pi_tool_result',args:{name:'read',args:{path:'a'}}});
        h.dispatch.dispatch(1,false,true,JSON.stringify(envelope(details)));
        assert.deepEqual(JSON.parse(JSON.stringify(await result)),envelope(details));
    }
});
test('native errors remain structured data and images retain MIME/data', async () => {
    const h = host(); const result = vm.runInContext("tool.result('read')", h.context);
    const value = envelope(null); value.isError = true;
    value.content.push({type:'image',data:'aW1hZ2U=',mimeType:'image/png'});
    h.dispatch.dispatch(1,false,true,JSON.stringify(value));
    assert.deepEqual(JSON.parse(JSON.stringify(await result)),value);
});
test('transport and policy denial reject, with no fallback or replay', async () => {
    const h = host(); const result = vm.runInContext("tool.result('bash', {command:'true'})", h.context);
    h.dispatch.dispatch(1,false,false,'EVAL_BRIDGE_DENIED');
    await assert.rejects(result,/EVAL_BRIDGE_DENIED/);
    assert.equal(h.requests.length,1);
});
test('invalid result envelopes reject rather than silently returning strings', async () => {
    for (const value of ['bad-json','null','[]','{}',JSON.stringify({...envelope(null),schema:'wrong'}),
        JSON.stringify({schema:'pi.eval.tool_result.v1',content:[],isError:false}),
        JSON.stringify({...envelope(null),isError:0}),JSON.stringify({...envelope(null),content:'text'})]) {
        const h = host(); const result = vm.runInContext("tool.result('read')", h.context);
        h.dispatch.dispatch(1,false,true,value);
        await assert.rejects(result,/EVAL_BRIDGE_PROTOCOL/);
    }
});
test('invalid request arguments do not enqueue a call', async () => {
    for (const expression of ["tool.result(4)","tool.result('read',null)","tool.result('read',[])","tool.result('read','x')"]) {
        const h = host(); await assert.rejects(vm.runInContext(expression,h.context),/requires a tool name/);
        assert.equal(h.requests.length,0);
    }
});
test('replacement of guest intrinsics does not replace result decoding', async () => {
    const h = host(); const result = vm.runInContext("tool.result('read')",h.context);
    vm.runInContext("JSON.parse=()=>{throw Error('guest parser')}; Array.isArray=()=>false; Object.prototype.hasOwnProperty=()=>false",h.context);
    h.dispatch.dispatch(1,false,true,JSON.stringify(envelope({ok:42})));
    assert.equal((await result).details.ok,42);
});
test('out-of-order replies keep text and result modes attached to the right call', async () => {
    const h = host();
    const a = vm.runInContext("tool.call('read')",h.context);
    const b = vm.runInContext("tool.result('read')",h.context);
    h.dispatch.dispatch(2,false,true,JSON.stringify(envelope('second')));
    h.dispatch.dispatch(1,false,true,'first');
    assert.equal(await a,'first'); assert.equal((await b).details,'second');
    h.dispatch.dispatch(2,false,false,'late duplicate');
});

const jobSnapshot = (id, status = 'exited', result = {content:[{type:'text',text:'child completed'}], details:{results:[{data:{count:7}}]}}) =>
    ({schema:'pi.host_job.v1', id, status, ...(status === 'running' ? {} : {result})});
async function launchAgent(h, variable, id) {
    const pending = vm.runInContext(`agent('inspect the code',{agent:'scout'}).then(value=>globalThis.${variable}=value)`,h.context);
    const request = h.requests.at(-1);
    assert.deepEqual(request.args, {name:'subagent',args:{agent:'scout',task:'inspect the code',async:true}});
    h.dispatch.dispatch(request.id,false,true,JSON.stringify(envelope({schema:'pi.subagent.background.v1',jobId:id})));
    await pending;
}
test('agent handles survive a cell boundary while each operation reenters the host', async () => {
    const h = host();
    await launchAgent(h,'saved','job-native');
    h.dispatch.clear();
    const waiting = vm.runInContext('saved.wait(10)',h.context);
    const request = h.requests.at(-1);
    assert.deepEqual(request.args,{name:'jobs',args:{action:'wait',jobId:'job-native',timeoutMs:10000}});
    h.dispatch.dispatch(request.id,false,true,JSON.stringify(envelope(jobSnapshot('job-native'))));
    const native = await waiting;
    assert.equal(native.details.results[0].data.count,7);
    const denied = vm.runInContext('saved.status()',h.context);
    const lookup = h.requests.at(-1);
    h.dispatch.dispatch(lookup.id,false,false,'PI_JOBS_UNKNOWN_ID');
    await assert.rejects(denied,/PI_JOBS_UNKNOWN_ID/);
});
test('agent options cannot override the operation or remove explicit backgrounding', async () => {
    for (const option of ['async','task','op','operation','model','__proto__']) {
        const h = host();
        const expression = `agent('inspect',JSON.parse(${JSON.stringify(JSON.stringify({[option]:'override'}))}))`;
        await assert.rejects(vm.runInContext(expression,h.context),/unknown option/);
        assert.equal(h.requests.length,0);
    }
});
test('an agent wait timeout leaves the job running without an implicit cancel or replay', async () => {
    const h = host();
    await launchAgent(h,'saved','job-running');
    const waiting = vm.runInContext('saved.wait(0)',h.context);
    const request = h.requests.at(-1);
    h.dispatch.dispatch(request.id,false,true,JSON.stringify(envelope(jobSnapshot('job-running','running'))));
    await assert.rejects(waiting,/EVAL_AGENT_TIMEOUT/);
    assert.equal(h.requests.length,2);
    assert.equal(request.args.args.action,'wait');
});
test('wait preserves requested handle order and retains native failed results when asked', async () => {
    const h = host();
    await launchAgent(h,'a','job-a');
    await launchAgent(h,'b','job-b');
    const waiting = vm.runInContext('wait([b,a],{timeout:10,raiseErrors:false})',h.context);
    let request = h.requests.at(-1);
    assert.equal(request.args.args.jobId,'job-b');
    const failed = {content:[{type:'text',text:'child failed'}],details:{failure:'fixture'},isError:true};
    h.dispatch.dispatch(request.id,false,true,JSON.stringify(envelope(jobSnapshot('job-b','failed',failed))));
    await new Promise(resolve=>setImmediate(resolve));
    request = h.requests.at(-1);
    assert.equal(request.args.args.jobId,'job-a');
    assert.ok(request.args.args.timeoutMs <= 10000);
    h.dispatch.dispatch(request.id,false,true,JSON.stringify(envelope(jobSnapshot('job-a'))));
    const results = await waiting;
    assert.equal(results[0].details.failure,'fixture');
    assert.equal(results[0].isError,true);
    assert.equal(results[1].details.results[0].data.count,7);
});

const workerSnapshot = (workerId, turnId = 1, status = 'running', result = null) => ({
    schema:'pi.subagent.worker.v1', workerId, jobId:`carrier-${workerId}`, agent:'scout',
    status:status === 'completed' ? 'idle' : status === 'cancelled' ? 'closed' : 'running',
    turnId, turn:{id:turnId,status,result},
});
const turnResult = turn => ({content:[{type:'text',text:`turn ${turn}`}],details:{turn}});
const answer = (h, request, details) => h.dispatch.dispatch(request.id,false,true,JSON.stringify(envelope(details)));
async function launchWorker(h, variable = 'worker', workerId = 'worker-native') {
    const pending = vm.runInContext(`agent('inspect the code',{agent:'scout',keepAlive:true,isolation:'none'}).then(value=>globalThis.${variable}=value)`,h.context);
    const request = h.requests.at(-1);
    assert.deepEqual(request.args,{name:'subagent',args:{agent:'scout',isolation:'none',task:'inspect the code',operation:'worker_start'}});
    answer(h,request,workerSnapshot(workerId));
    await pending;
}
test('a warm worker survives cells and send returns a pinned turn while worker wait selects latest', async () => {
    const h = host();
    await launchWorker(h);
    h.dispatch.clear();
    let pending = vm.runInContext("worker.send('follow up').then(value=>globalThis.second=value)",h.context);
    let request = h.requests.at(-1);
    assert.deepEqual(request.args,{name:'subagent',args:{operation:'worker_send',workerId:'worker-native',task:'follow up'}});
    answer(h,request,workerSnapshot('worker-native',2,'queued'));
    await pending;
    assert.equal(vm.runInContext('worker.turnId',h.context),2);
    assert.equal(vm.runInContext('second.turnId',h.context),2);
    pending = vm.runInContext("worker.send('another').then(value=>globalThis.third=value)",h.context);
    answer(h,h.requests.at(-1),workerSnapshot('worker-native',3,'queued'));
    await pending;
    for (const [expression,turnId] of [['second.wait(4)',2],['worker.wait(4)',3]]) {
        pending = vm.runInContext(expression,h.context);
        request = h.requests.at(-1);
        assert.deepEqual(request.args,{name:'subagent',args:{operation:'worker_wait',workerId:'worker-native',turnId,timeoutMs:4000}});
        answer(h,request,workerSnapshot('worker-native',turnId,'completed',turnResult(turnId)));
        assert.equal((await pending).details.turn,turnId);
    }
});
test('concurrent warm sends keep the newest turn selected even when acknowledgments arrive out of order', async () => {
    const h = host();
    await launchWorker(h);
    const first = vm.runInContext("worker.send('second')",h.context);
    const a = h.requests.at(-1);
    const second = vm.runInContext("worker.send('third')",h.context);
    const b = h.requests.at(-1);
    answer(h,b,workerSnapshot('worker-native',3,'queued'));
    assert.equal((await second).turnId,3);
    answer(h,a,workerSnapshot('worker-native',2,'queued'));
    assert.equal((await first).turnId,2);
    assert.equal(vm.runInContext('worker.turnId',h.context),3);
});
test('mixed wait captures worker turns before awaiting other jobs and retains failure data', async () => {
    const h = host();
    await launchAgent(h,'job','job-once');
    await launchWorker(h);
    const waiting = vm.runInContext('wait([job,worker],{timeout:10,raiseErrors:false})',h.context);
    const jobRequest = h.requests.at(-1);
    const sending = vm.runInContext("worker.send('new turn')",h.context);
    answer(h,h.requests.at(-1),workerSnapshot('worker-native',2,'queued'));
    await sending;
    answer(h,jobRequest,jobSnapshot('job-once'));
    await new Promise(resolve=>setImmediate(resolve));
    const turnRequest = h.requests.at(-1);
    assert.equal(turnRequest.args.args.turnId,1);
    assert.ok(turnRequest.args.args.timeoutMs <= 10000);
    const failure = {content:[{type:'text',text:'turn failed'}],details:{turn:1},isError:true};
    answer(h,turnRequest,workerSnapshot('worker-native',1,'failed',failure));
    const results = await waiting;
    assert.equal(results[0].details.results[0].data.count,7);
    assert.equal(results[1].isError,true);
    assert.equal(vm.runInContext('worker.turnId',h.context),2);
});
test('worker timeout does not cancel and turn cancellation explicitly closes the whole worker', async () => {
    const h = host();
    await launchWorker(h);
    const waiting = vm.runInContext('worker.wait(0)',h.context);
    answer(h,h.requests.at(-1),workerSnapshot('worker-native'));
    await assert.rejects(waiting,/EVAL_AGENT_TIMEOUT/);
    assert.equal(h.requests.length,2);
    const sending = vm.runInContext("worker.send('later').then(value=>globalThis.turn=value)",h.context);
    answer(h,h.requests.at(-1),workerSnapshot('worker-native',2,'queued'));
    await sending;
    for (const expression of ['turn.cancel()','worker.close()']) {
        const pending = vm.runInContext(expression,h.context);
        const request = h.requests.at(-1);
        assert.deepEqual(request.args,{name:'subagent',args:{operation:'worker_close',workerId:'worker-native'}});
        answer(h,request,workerSnapshot('worker-native',2,'cancelled',{
            content:[{type:'text',text:'cancelled'}],details:null,isError:true,
        }));
        assert.equal((await pending).status,'closed');
    }
});
test('warm options reject unsupported or ambiguous execution before dispatch', async () => {
    for (const options of [{keepAlive:1},{keepAlive:'true'},{keepAlive:null},
        {keepAlive:true,isolation:'worktree'},{keepAlive:true,isoApply:false},
        {keepAlive:true,outputSchema:{}},{keepAlive:true,schemaMode:'strict'},
        {keepAlive:true,op:'worker_close'},{keepAlive:true,workerId:'foreign'}]) {
        const h = host();
        await assert.rejects(vm.runInContext(`agent('inspect',${JSON.stringify(options)})`,h.context),/EVAL_AGENT_INPUT/);
        assert.equal(h.requests.length,0);
    }
    const h = host();
    await launchWorker(h);
    for (const expression of ["worker.send('')","worker.send('  ')",'worker.send(4)']) {
        await assert.rejects(vm.runInContext(expression,h.context),/EVAL_AGENT_INPUT/);
    }
    assert.equal(h.requests.length,1);
});
test('worker reply validation rejects changed identity, turn substitution and missing terminal result', async () => {
    const h = host();
    await launchWorker(h);
    for (const malformed of [workerSnapshot('other'),workerSnapshot('worker-native',2),
        {...workerSnapshot('worker-native'),jobId:'different'},
        {...workerSnapshot('worker-native'),agent:'different'},
        {...workerSnapshot('worker-native'),turn:{id:1,status:'unknown',result:null}},
        workerSnapshot('worker-native',1,'completed',null)]) {
        const pending = vm.runInContext('worker.wait(0)',h.context);
        answer(h,h.requests.at(-1),malformed);
        await assert.rejects(pending,/EVAL_AGENT_PROTOCOL/);
    }
    const denied = vm.runInContext('worker.status()',h.context);
    h.dispatch.dispatch(h.requests.at(-1).id,false,false,'PI_WORKER_OWNER_CHANGED');
    await assert.rejects(denied,/PI_WORKER_OWNER_CHANGED/);
});

function poolHost(h) {
    const workers = new Map();
    const operations = [];
    const reply = request => {
        assert.equal(request.args.name,'subagent');
        const args = request.args.args;
        operations.push(args.operation);
        let worker;
        if (args.operation === 'worker_start') {
            const id = `pool-worker-${workers.size + 1}`;
            worker = {id,agent:args.agent,status:'running',latest:1,turns:new Map([[1,{task:args.task,status:'running',result:null}]])};
            workers.set(id,worker);
        } else {
            worker = workers.get(args.workerId);
            assert.ok(worker,'operation must reference an acknowledged worker');
            if (args.operation === 'worker_send') {
                worker.latest++;
                worker.status = 'running';
                worker.turns.set(worker.latest,{task:args.task,status:'queued',result:null});
            } else if (args.operation === 'worker_close') {
                worker.status = 'closed';
                for (const turn of worker.turns.values()) if (turn.result === null) {
                    turn.status = 'cancelled';
                    turn.result = {content:[],details:{cancelled:true},isError:true};
                }
            } else assert.ok(['worker_status','worker_wait'].includes(args.operation));
        }
        const turnId = args.turnId ?? worker.latest;
        const turn = worker.turns.get(turnId);
        answer(h,request,{schema:'pi.subagent.worker.v1',workerId:worker.id,jobId:`carrier-${worker.id}`,
            agent:worker.agent,status:worker.status,turnId,turn:{id:turnId,status:turn.status,result:turn.result}});
    };
    return {workers,operations,reply};
}
async function drive(h, pending, reply, start = h.requests.length) {
    let settled = false;
    pending.then(() => { settled = true; },() => { settled = true; });
    let cursor = start;
    for (let ticks = 0; !settled && ticks < 200; ticks++) {
        await new Promise(resolve=>setImmediate(resolve));
        while (cursor < h.requests.length) reply(h.requests[cursor++]);
    }
    assert.ok(settled,'controlled host must settle the pool operation');
    return pending;
}
async function createPool(h, options = {agent:'scout',concurrency:2}) {
    await vm.runInContext(`workpool(${JSON.stringify(options)}).then(value=>globalThis.pool=value)`,h.context);
}
test('workpool lazily creates bounded workers, reuses their FIFO turns and survives cells', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    assert.equal(h.requests.length,0);
    const handles = await drive(h,vm.runInContext("pool.push(['a','b','c','d','e'])",h.context),native.reply);
    assert.deepEqual(native.operations,['worker_start','worker_start','worker_send','worker_send','worker_send']);
    assert.deepEqual(Array.from(handles,turn=>[turn.workerId,turn.turnId]),[
        ['pool-worker-1',1],['pool-worker-2',1],['pool-worker-1',2],['pool-worker-2',2],['pool-worker-1',3],
    ]);
    h.dispatch.clear();
    const idle = native.workers.get('pool-worker-1');
    idle.status = 'idle';
    for (const [id,turn] of idle.turns) { turn.status = 'completed'; turn.result = turnResult(id); }
    const followup = await drive(h,vm.runInContext("pool.push(['f'])",h.context),native.reply);
    assert.equal(followup[0].workerId,'pool-worker-1','idle worker is preferred before balancing total admissions');
    assert.equal(native.workers.size,2);
    assert.deepEqual(native.operations.slice(-3),['worker_status','worker_status','worker_send']);
    const peek = await drive(h,vm.runInContext('pool.peek()',h.context),native.reply);
    assert.equal(peek.items.length,6);
    assert.equal(peek.items[0].result.details.turn,1);
    assert.equal(peek.items[5].turnId,4);
    const status = await drive(h,vm.runInContext('pool.status()',h.context),native.reply);
    assert.equal(status.total,6);
    assert.equal(status.counts.completed,3);
    assert.equal(status.workerCount,2);
});
test('workpool validates every task before launch and enforces a lifetime 32-item bound', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    for (const tasks of [['valid',''],['valid',null],['valid','x\0y'],['valid','é'.repeat(32769)],['valid','\ud800']]) {
        await assert.rejects(vm.runInContext(`pool.push(${JSON.stringify(tasks)})`,h.context),/EVAL_POOL_INPUT/);
        assert.equal(h.requests.length,0);
    }
    const handles = await drive(h,vm.runInContext('pool.push(Array.from({length:32},(_,n)=>`task ${n}`))',h.context),native.reply);
    assert.equal(handles.length,32);
    assert.equal(h.requests.length,32,'one native admission per item, not polling before every send');
    assert.equal(native.workers.size,2);
    await assert.rejects(vm.runInContext("pool.push(['over capacity'])",h.context),/EVAL_POOL_CAPACITY/);
    assert.equal(h.requests.length,32);
    for (const options of [{concurrency:0},{concurrency:9},{concurrency:1.5},{concurrency:true},
        {isolation:'worktree'},{timeoutSeconds:0},{scope:'all'},{keepAlive:false},{outputSchema:{}},{cwd:null}]) {
        await assert.rejects(vm.runInContext(`workpool(${JSON.stringify(options)})`,h.context),/EVAL_POOL_INPUT/);
    }
    assert.equal(h.requests.length,32);
});
test('workpool partial admission failure retains handles, closes admission and allows cleanup', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    let admissions = 0;
    const reply = request => {
        if (['worker_start','worker_send'].includes(request.args.args.operation) && ++admissions === 3) {
            h.dispatch.dispatch(request.id,false,false,'PI_WORKER_REJECTED');
        } else native.reply(request);
    };
    await assert.rejects(drive(h,vm.runInContext("pool.push(['a','b','c','d'])",h.context),reply),error=>{
        assert.match(error.message,/acknowledged 2 of 4/);
        assert.equal(error.admitted.length,2);
        assert.equal(error.requested,4);
        return true;
    });
    assert.equal(vm.runInContext('pool.items.length',h.context),2);
    await assert.rejects(vm.runInContext("pool.push(['retry'])",h.context),/EVAL_POOL_CLOSED/);
    const peek = await drive(h,vm.runInContext('pool.peek()',h.context),native.reply);
    assert.equal(peek.items.length,2);
    assert.equal(peek.closed,true);
    assert.match(peek.lastError,/PI_WORKER_REJECTED/);
    const closed = await drive(h,vm.runInContext('pool.close()',h.context),native.reply);
    assert.equal(closed.allClosed,true);
    assert.equal(closed.workers.length,2);
});
test('workpool serializes push then close and refuses later admission without native dispatch', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h,{agent:'scout',concurrency:1});
    const pending = vm.runInContext("Promise.allSettled([pool.push(['a','b']),pool.close(),pool.push(['too late'])])",h.context);
    const settled = await drive(h,pending,native.reply);
    assert.equal(settled[0].status,'fulfilled');
    assert.equal(settled[1].value.allClosed,true);
    assert.match(settled[2].reason.message,/EVAL_POOL_CLOSED/);
    assert.deepEqual(native.operations,['worker_start','worker_send','worker_close']);
    assert.equal((await vm.runInContext('pool.close()',h.context)).allClosed,true);
    assert.equal(h.requests.length,3,'acknowledged closes are not replayed');
    const empty = host(); await createPool(empty);
    assert.equal((await vm.runInContext('pool.close()',empty.context)).allClosed,true);
    assert.equal(empty.requests.length,0);
});
test('an interrupted pool command can close known workers without retaining a stuck guest queue', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h,{agent:'scout',concurrency:1});
    vm.runInContext("pool.push(['a','b'])",h.context);
    await new Promise(resolve=>setImmediate(resolve));
    native.reply(h.requests[0]);
    await new Promise(resolve=>setImmediate(resolve));
    assert.equal(h.requests[1].args.args.operation,'worker_send');
    h.dispatch.clear();
    const closed = await drive(h,vm.runInContext('pool.close()',h.context),native.reply);
    assert.equal(closed.allClosed,true);
    assert.equal(closed.workers.length,1);
    assert.equal(vm.runInContext('pool.items.length',h.context),1);
    assert.equal(h.requests.length,3,'interrupted send is not replayed');
});
test('workpool reports uncertainty after a lost start acknowledgment instead of claiming full cleanup', async () => {
    const h = host(); await createPool(h);
    await assert.rejects(drive(h,vm.runInContext("pool.push(['first'])",h.context),request=>{
        h.dispatch.dispatch(request.id,false,false,'acknowledgment lost');
    }),/EVAL_POOL_ADMISSION/);
    const closed = await vm.runInContext('pool.close()',h.context);
    assert.equal(closed.allKnownClosed,true);
    assert.equal(closed.unknownAdmission,true);
    assert.equal(closed.allClosed,false);
    assert.equal(h.requests.length,1);
});
