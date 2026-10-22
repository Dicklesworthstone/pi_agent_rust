'use strict';
// Diagnostics for the actual embedded JS factory, not a native/DSR substitute.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../src/eval/js_kernel/async_host.js'), 'utf8');
function host(globals = {}) {
    let next = 1;
    const requests = [];
    const context = vm.createContext({...globals,tool:{},
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

const modelAck = (operation, jobId, timeoutSeconds = operation === 'judgment_batch' ? 300 : 60) =>
    ({schema:'pi.eval.model-job.v1',operation,jobId,timeoutSeconds});
const judgmentValue = value => ({schema:'pi.judgment.v1',answers:{safe:{type:'bool',bool:value}}});
const modelResult = (operation, result, failed = false, error = null) => ({
    content:[{type:'text',text:failed ? 'model request failed' : 'model request completed'}],
    details:{schema:'pi.eval.model-job.result.v1',operation,result,error,
        judgments:{schema:'pi.eval.judgments.v1',attempts:[{attempt:1,status:failed ? 'failed' : 'completed'}]}},
    ...(failed ? {isError:true} : {}),
});
const modelSnapshot = (id, operation, result, status = 'exited') =>
    ({...jobSnapshot(id,status,result),command:operation === 'completion' ? 'eval model completion' : `eval ${operation}`});
const batchProgressResult = (interrupted = false) => {
    const items = [{key:'ready',status:'completed',verdict:judgmentValue(1)},
        {key:'bad',status:'failed',error:'PI_JUDGE_PROVIDER'},
        {key:'active',status:'running'},{key:'later',status:'queued'}];
    const partialResult = {content:[{type:'text',text:'2 of 4 judgments completed'}],details:{
        schema:'pi.eval.model-job.progress.v1',operation:'judgment_batch',complete:false,
        total:4,completed:2,succeeded:1,failed:1,queued:1,running:1,items,
        judgments:{schema:'pi.eval.judgments.v1',attempts:items.map((item,index) =>
            ({attempt:index + 1,operation:'judgment',status:item.status,usage:null,usageKnown:false}))},
    }};
    return {content:[{type:'text',text:interrupted ? 'job interrupted' : 'job running'}],
        details:{schema:interrupted ? 'pi.host_job.interrupted.v1' : 'pi.host_job.progress.v1',partialResult},
        ...(interrupted ? {isError:true} : {})};
};
const batchProgressSnapshot = (native, status = 'running', id = 'model-native') =>
    ({...modelSnapshot(id,'judgment_batch',native,status),result:native});
const startProgressBatch = h => launchModel(h,
    "judgeBatch({ready:1,bad:2,active:3,later:4},{safe:{type:'bool',instructions:'safe?'}})",'judgment_batch');
async function launchModel(h, expression, operation, id = 'model-native', variable = 'model') {
    const pending = vm.runInContext(`${expression}.then(value=>globalThis.${variable}=value)`,h.context);
    const request = h.requests.at(-1);
    assert.equal(request.name,{completion:'__pi_model_complete_start',judgment:'__pi_judge_start',
        judgment_batch:'__pi_judge_batch_start'}[operation]);
    h.dispatch.dispatch(request.id,false,true,JSON.stringify(modelAck(operation,id,
        request.args.timeout_secs ?? (operation === 'judgment_batch' ? 300 : 60))));
    await pending;
    return request;
}
test('model factories return native handles and preserve explicit options and image evidence', async () => {
    const h = host();
    const request = await launchModel(h,
        "completion('summarize',{model:'smol',system:'be concise',schema:{type:'object'},images:['chart.png'],maxTokens:256,timeoutSeconds:17})",
        'completion');
    assert.deepEqual(request.args,{prompt:'summarize',model:'smol',system:'be concise',schema:{type:'object'},images:['chart.png'],max_tokens:256,timeout_secs:17});
    assert.equal(vm.runInContext('model.operation',h.context),'completion');
    assert.equal(vm.runInContext('model.id === model.jobId && Object.isFrozen(model)',h.context),true);
    h.dispatch.clear();
    const waiting = vm.runInContext('model.wait(5)',h.context);
    const payload = {schema:'pi.completion.v1',text:'{"answer":42}',data:{answer:42},schemaValid:true};
    const lookup = h.requests.at(-1);
    assert.deepEqual(lookup.args,{name:'jobs',args:{action:'wait',jobId:'model-native',timeoutMs:5000}});
    answer(h,lookup,modelSnapshot('model-native','completion',modelResult('completion',payload)));
    assert.deepEqual(JSON.parse(JSON.stringify(await waiting)),{answer:42});
    const single = await launchModel(h,"judge({view:'screen'},{safe:{type:'bool',instructions:'safe?'}},{model:'slow',images:['shot.png'],timeoutSeconds:8})",'judgment','judge-native','review');
    assert.equal(single.args.model,'slow');
    assert.deepEqual(single.args.images,['shot.png']);
    assert.equal(single.args.timeout_secs,8);
    const batch = await launchModel(h,"judgeBatch([{id:1},{id:2}],{safe:{type:'bool',instructions:'safe?'}},{model:'tiny',images:{'1':['b.png']},concurrency:2})",'judgment_batch','batch-native','batch');
    assert.equal(batch.args.model,'tiny');
    assert.deepEqual(batch.args.images,{'1':['b.png']});
    assert.equal(batch.args.concurrency,2);
    assert.equal(h.requests.length,4);
});
test('model waits preserve false, zero and null structured answers and full native receipts', async () => {
    for (const data of [null,false,0]) {
        const h = host();
        await launchModel(h,"completion('extract',{schema:true})",'completion');
        const payload = {schema:'pi.completion.v1',text:JSON.stringify(data),data,schemaValid:true};
        const native = modelResult('completion',payload);
        let pending = vm.runInContext('model.wait()',h.context);
        answer(h,h.requests.at(-1),modelSnapshot('model-native','completion',native));
        assert.equal(await pending,data);
        pending = vm.runInContext('model.output()',h.context);
        answer(h,h.requests.at(-1),modelSnapshot('model-native','completion',native));
        assert.deepEqual(JSON.parse(JSON.stringify(await pending)),native);
    }
    const h = host();
    await launchModel(h,"judge('evidence',{safe:{type:'bool',instructions:'safe?'}})",'judgment');
    const pending = vm.runInContext('model.wait()',h.context);
    answer(h,h.requests.at(-1),modelSnapshot('model-native','judgment',modelResult('judgment',judgmentValue(0))));
    assert.equal((await pending).safe.bool,0);
});
test('mixed judgment batch failures retain ordered successful verdicts and error receipts', async () => {
    const h = host();
    await launchModel(h,"judgeBatch(['good','bad'],{safe:{type:'bool',instructions:'safe?'}})",'judgment_batch');
    const payload = {schema:'pi.eval.judgment-batch.v1',batch:1,total:2,succeeded:1,failed:1,allOk:false,
        items:[{key:0,ok:true,verdict:judgmentValue(1)},{key:1,ok:false,error:'PI_JUDGE_PROVIDER'}]};
    const native = modelResult('judgment_batch',payload,true);
    let pending = vm.runInContext('model.wait(0,false)',h.context);
    answer(h,h.requests.at(-1),modelSnapshot('model-native','judgment_batch',native,'failed'));
    assert.deepEqual(JSON.parse(JSON.stringify(await pending)),payload);
    pending = vm.runInContext('model.wait(0)',h.context);
    answer(h,h.requests.at(-1),modelSnapshot('model-native','judgment_batch',native,'failed'));
    await assert.rejects(pending,error => error.result.details.result.items[0].verdict.answers.safe.bool === 1
        && error.jobId === 'model-native');
    pending = vm.runInContext('model.output()',h.context);
    answer(h,h.requests.at(-1),modelSnapshot('model-native','judgment_batch',native,'failed'));
    assert.deepEqual(JSON.parse(JSON.stringify(await pending)),native);
});
test('batch progress exposes ordered evidence and each observation reenters the host', async () => {
    const h = host(); await startProgressBatch(h); h.dispatch.clear();
    const native = batchProgressResult(); const snapshot = batchProgressSnapshot(native);
    const progress = native.details.partialResult.details;
    for (const [method,expected] of [['progress',{...progress,jobStatus:'running'}],
        ['status',snapshot],['output',native],['done',false]]) {
        const pending = vm.runInContext(`model.${method}()`,h.context);
        assert.deepEqual(h.requests.at(-1).args,{name:'jobs',args:{action:'wait',jobId:'model-native',timeoutMs:0}});
        answer(h,h.requests.at(-1),snapshot);
        assert.deepEqual(JSON.parse(JSON.stringify(await pending)),expected);
    }
    assert.equal(h.requests.length,5);
});
test('partial batch results never satisfy either individual or global waits', async () => {
    const h = host(); await startProgressBatch(h);
    for (const expression of ['model.wait(0)','model.wait(0,false)',
        'wait([model],{timeout:0,raiseErrors:false})']) {
        const pending = vm.runInContext(expression,h.context);
        answer(h,h.requests.at(-1),batchProgressSnapshot(batchProgressResult()));
        await assert.rejects(pending,error => error.name === 'TimeoutError');
    }
    assert.equal(h.requests.filter(request => request.name === '__pi_judge_batch_start').length,1);
    assert.ok(h.requests.slice(1).every(request => request.args.args.action === 'wait'));
});
test('cancelled and timed-out batches retain completed evidence without claiming remaining work continues', async () => {
    for (const status of ['killed','timedOut']) {
        const h = host(); await startProgressBatch(h);
        const native = batchProgressResult(true); const snapshot = batchProgressSnapshot(native,status);
        // A generic wrapper field must never be mistaken for a typed final payload.
        native.details.result = 'unrelated wrapper metadata';
        const progress = {...native.details.partialResult.details,jobStatus:status};
        for (const [expression,expected] of [['model.progress()',progress],['model.done()',true],
            ['model.output()',native],['model.wait(0,false)',native],
            ['wait([model],{timeout:0,raiseErrors:false})',[native]]]) {
            const pending = vm.runInContext(expression,h.context);
            answer(h,h.requests.at(-1),snapshot);
            assert.deepEqual(JSON.parse(JSON.stringify(await pending)),expected);
        }
        const raising = vm.runInContext('model.wait(0)',h.context);
        answer(h,h.requests.at(-1),snapshot);
        await assert.rejects(raising,error => error.jobId === 'model-native'
            && error.result.details.partialResult.details.items[0].verdict.answers.safe.bool === 1);
        if (status === 'killed') {
            const cancelled = vm.runInContext('model.cancel()',h.context);
            assert.equal(h.requests.at(-1).args.args.action,'cancel');
            answer(h,h.requests.at(-1),snapshot);
            assert.equal((await cancelled).status,'killed');
        }
    }
});
test('settled batch progress derives complete counts and ordered items from the final aggregate', async () => {
    for (const failed of [false,true]) {
        const h = host(); await startProgressBatch(h);
        const items = [{key:'first',ok:true,verdict:judgmentValue(1)},failed
            ? {key:'second',ok:false,error:'PI_JUDGE_PROVIDER'} : {key:'second',ok:true,verdict:judgmentValue(0)}];
        const payload = {schema:'pi.eval.judgment-batch.v1',total:2,succeeded:failed ? 1 : 2,
            failed:failed ? 1 : 0,allOk:!failed,items};
        const native = modelResult('judgment_batch',payload,failed);
        native.details.judgments.attempts = items.map((item,index) =>
            ({attempt:index + 1,status:item.ok ? 'completed' : 'failed'}));
        const status = failed ? 'failed' : 'exited';
        const pending = vm.runInContext('model.progress()',h.context);
        answer(h,h.requests.at(-1),modelSnapshot('model-native','judgment_batch',native,status));
        const progress = JSON.parse(JSON.stringify(await pending));
        assert.deepEqual(progress,{schema:'pi.eval.model-job.progress.v1',operation:'judgment_batch',
            complete:true,jobStatus:status,total:2,completed:2,succeeded:payload.succeeded,failed:payload.failed,
            queued:0,running:0,items:[{key:'first',status:'completed',verdict:judgmentValue(1)},failed
                ? {key:'second',status:'failed',error:'PI_JUDGE_PROVIDER'}
                : {key:'second',status:'completed',verdict:judgmentValue(0)}],judgments:native.details.judgments});
    }
});
test('batch progress returns null before the first update and for failures without retained evidence', async () => {
    const h = host(); await startProgressBatch(h);
    const failed = {content:[],details:null,isError:true};
    for (const snapshot of [modelSnapshot('model-native','judgment_batch',undefined,'running'),
        batchProgressSnapshot(null),batchProgressSnapshot(failed,'failed'),batchProgressSnapshot(failed,'killed'),
        batchProgressSnapshot(modelResult('judgment_batch',null,true,'PI_JUDGE_PRIVACY'),'failed')]) {
        const pending = vm.runInContext('model.progress()',h.context);
        answer(h,h.requests.at(-1),snapshot); assert.equal(await pending,null);
    }
});
test('batch attachment recovers partial results after reset without caching old access', async () => {
    const h = host(); const snapshot = batchProgressSnapshot(batchProgressResult(),'running','recovered');
    const attached = vm.runInContext("judgeBatch.attach('recovered').then(value=>globalThis.model=value)",h.context);
    answer(h,h.requests.at(-1),snapshot); await attached;
    h.dispatch.clear();
    let pending = vm.runInContext('model.progress()',h.context);
    answer(h,h.requests.at(-1),snapshot);
    assert.equal((await pending).completed,2);
    pending = vm.runInContext('model.progress()',h.context);
    h.dispatch.dispatch(h.requests.at(-1).id,false,false,'PI_JOBS_UNKNOWN_ID');
    await assert.rejects(pending,/PI_JOBS_UNKNOWN_ID/);
    assert.equal(h.requests.length,3);
    assert.ok(h.requests.every(request => request.name === '__pi_tool_result'));
});
test('batch progress validates counts, unique keys, evidence and receipt shapes', async () => {
    const h = host(); await startProgressBatch(h);
    const corruptions = [
        (n,p) => { n.details.schema = 'wrong'; }, (n,p) => { n.isError = true; },
        (n,p) => { n.details.partialResult.isError = true; }, (n,p) => { n.details.partialResult.content = null; },
        (n,p) => { p.schema = 'wrong'; }, (n,p) => { p.operation = 'completion'; },
        (n,p) => { p.complete = true; }, (n,p) => { p.total = 5; },
        (n,p) => { p.completed = 3; }, (n,p) => { p.succeeded = true; },
        (n,p) => { p.running = -1; }, (n,p) => { p.items[3].key = 'ready'; },
        (n,p) => { p.items[3].key = ''; }, (n,p) => { p.items[3].key = false; },
        (n,p) => { p.items[0].status = ['completed']; }, (n,p) => { p.items[0].verdict = null; },
        (n,p) => { p.items[0].error = 'contradiction'; }, (n,p) => { p.items[1].error = ''; },
        (n,p) => { p.items[1].verdict = judgmentValue(1); }, (n,p) => { p.items[2].verdict = judgmentValue(1); },
        (n,p) => { p.items[3].error = null; }, (n,p) => { p.items[3].status = 'running'; },
        (n,p) => { p.judgments.schema = 'wrong'; }, (n,p) => { p.judgments.attempts.pop(); },
        (n,p) => { p.judgments.attempts[0] = null; },
    ];
    for (const corrupt of corruptions) {
        const native = batchProgressResult(); corrupt(native,native.details.partialResult.details);
        const pending = vm.runInContext('model.status()',h.context);
        answer(h,h.requests.at(-1),batchProgressSnapshot(native));
        await assert.rejects(pending,/EVAL_MODEL_PROTOCOL/);
    }
    const native = batchProgressResult();
    native.details.partialResult.details.items[0].key = 0;
    native.details.partialResult.details.items[1].key = '0';
    const pending = vm.runInContext('model.progress()',h.context);
    answer(h,h.requests.at(-1),batchProgressSnapshot(native));
    assert.deepEqual(Array.from((await pending).items,item => item.key),[0,'0','active','later']);
});
test('progress wrappers are restricted to batches and the matching running or interrupted states', async () => {
    const h = host(); await startProgressBatch(h);
    for (const [native,status] of [[batchProgressResult(true),'running'],[batchProgressResult(true),'exited'],
        [batchProgressResult(true),'failed'],[batchProgressResult(),'killed'],[batchProgressResult(),'timedOut']]) {
        const pending = vm.runInContext('model.status()',h.context);
        answer(h,h.requests.at(-1),batchProgressSnapshot(native,status));
        await assert.rejects(pending,/EVAL_MODEL_PROTOCOL/);
    }
    for (const [operation,expression] of [['completion',"completion('work')"],
        ['judgment',"judge('work',{safe:{type:'bool',instructions:'safe?'}})"]]) {
        const other = host(); await launchModel(other,expression,operation);
        await assert.rejects(vm.runInContext('model.progress()',other.context),/EVAL_MODEL_INPUT/);
        assert.equal(other.requests.length,1);
        const pending = vm.runInContext('model.status()',other.context);
        answer(other,other.requests.at(-1),{...modelSnapshot('model-native',operation,undefined,'running'),
            result:batchProgressResult()});
        await assert.rejects(pending,/EVAL_MODEL_PROTOCOL/);
    }
});
test('malformed and lost model acknowledgments reject without retrying admitted work', async () => {
    for (const response of ['not JSON','null',JSON.stringify(modelAck('judgment','model-native')),
        JSON.stringify({...modelAck('completion','model-native'),schema:'wrong'}),
        JSON.stringify(modelAck('completion','')),
        JSON.stringify(modelAck('completion','model-native',300))]) {
        const h = host(); const pending = vm.runInContext("completion('work')",h.context);
        h.dispatch.dispatch(1,false,true,response);
        await assert.rejects(pending,/EVAL_MODEL_PROTOCOL/);
        assert.equal(h.requests.length,1);
    }
    const h = host(); const pending = vm.runInContext("completion('work')",h.context);
    h.dispatch.dispatch(1,false,false,'ACK_LOST');
    await assert.rejects(pending,/ACK_LOST/);
    assert.equal(h.requests.length,1);
});
test('model helpers reject invalid role, retry, threshold and authority options before dispatch', async () => {
    for (const expression of ["completion('work',{model:'other'})","completion('work',{model:'Slow'})",
        "completion('work',{model:' smol'})","completion('work',{model:'provider/model'})",
        "completion('work',{model:'__proto__'})","completion('work',{model:null})",
        "judge('state',{q:{}},{model:{role:'slow'}})","judgeBatch(['state'],{q:{}},{model:1})",
        "completion('work',{api_key:'secret'})",
        "completion('work',{maxTokens:0})","completion('work',{timeoutSeconds:601})",
        "judge('state',{q:{}},{retries:1})","judge('state',{q:{}},{operation:'other'})",
        "judgeBatch(['state'],{q:{}},{minOk:1})","judgeBatch(['state'],{q:{}},{intent:'custom'})",
        "judgeBatch(['state'],{q:{}},{concurrency:9})","judgeBatch([],{q:{}})",
        "completion.attach('invalid id')","completion('work',JSON.parse('{\"__proto__\":{}}'))"]) {
        const h = host(); await assert.rejects(vm.runInContext(expression,h.context),/EVAL_MODEL_INPUT/);
        assert.equal(h.requests.length,0);
    }
});
test('model roles are per launch and never replayed or inherited by later calls', async () => {
    const h = host();
    for (const role of ['default','smol','slow','plan','commit','vision','designer','task','advisor','tiny']) {
        const request = await launchModel(h,`completion('work',{model:${JSON.stringify(role)}})`,'completion');
        assert.equal(request.args.model,role);
    }
    const defaultRequest = await launchModel(h,"completion('cell binding')",'completion');
    assert.deepEqual(defaultRequest.args,{prompt:'cell binding'});
    const pending = vm.runInContext("completion('blocked',{model:'slow'})",h.context);
    h.dispatch.dispatch(h.requests.at(-1).id,false,false,'PI_COMPLETION_BINDING');
    await assert.rejects(pending,/PI_COMPLETION_BINDING/);
    assert.equal(h.requests.length,12);
});
test('model attachment after reset verifies native identity and uses current host authority', async () => {
    const h = host();
    const attach = vm.runInContext("completion.attach('recovered').then(value=>globalThis.model=value)",h.context);
    assert.deepEqual(h.requests[0].args,{name:'jobs',args:{action:'wait',jobId:'recovered',timeoutMs:0}});
    answer(h,h.requests[0],modelSnapshot('recovered','completion',undefined,'running'));
    await attach;
    h.dispatch.clear();
    let pending = vm.runInContext('model.output()',h.context);
    answer(h,h.requests.at(-1),modelSnapshot('recovered','completion',undefined,'running'));
    assert.equal(await pending,null);
    pending = vm.runInContext('model.done()',h.context);
    answer(h,h.requests.at(-1),modelSnapshot('recovered','completion',
        modelResult('completion',{schema:'pi.completion.v1',text:'done'})));
    assert.equal(await pending,true);
    pending = vm.runInContext('model.status()',h.context);
    h.dispatch.dispatch(h.requests.at(-1).id,false,false,'PI_JOBS_UNKNOWN_ID');
    await assert.rejects(pending,/PI_JOBS_UNKNOWN_ID/);
    assert.equal(h.requests.length,4);
    for (const changed of [{command:'eval judgment'}, {command:'eval completion'}, {command:'reusable subagent pool pool-id'},
        {id:'different'}, {status:'unknown'}]) {
        const rejected = vm.runInContext("completion.attach('recovered')",h.context);
        answer(h,h.requests.at(-1),{...modelSnapshot('recovered','completion',undefined,'running'),...changed});
        await assert.rejects(rejected,/PROTOCOL/);
    }
});
test('model timeouts and cancellation never fabricate an answer or replay generation', async () => {
    const h = host();
    await launchModel(h,"completion('work')",'completion');
    let pending = vm.runInContext('model.wait(0)',h.context);
    answer(h,h.requests.at(-1),modelSnapshot('model-native','completion',undefined,'running'));
    await assert.rejects(pending,error=>error.name === 'TimeoutError');
    const cancelled = {content:[{type:'text',text:'cancelled'}],details:null,isError:true};
    pending = vm.runInContext('model.cancel()',h.context);
    assert.equal(h.requests.at(-1).args.args.action,'cancel');
    answer(h,h.requests.at(-1),modelSnapshot('model-native','completion',cancelled,'killed'));
    assert.equal((await pending).status,'killed');
    pending = vm.runInContext('model.wait(0,false)',h.context);
    answer(h,h.requests.at(-1),modelSnapshot('model-native','completion',cancelled,'killed'));
    assert.deepEqual(JSON.parse(JSON.stringify(await pending)),cancelled);
    assert.equal(h.requests.filter(request=>request.name === '__pi_model_complete_start').length,1);
});
test('model terminal metadata and answer schemas must agree with the retained handle', async () => {
    for (const native of [modelResult('judgment',judgmentValue(1)),
        modelResult('completion',{schema:'pi.completion.v1',text:'null',data:null}),
        modelResult('completion',{schema:'wrong',text:'answer'}),
        modelResult('completion',null),
        {...modelResult('completion',{schema:'pi.completion.v1',text:'answer'}),details:null}]) {
        const h = host(); await launchModel(h,"completion('work')",'completion');
        const pending = vm.runInContext('model.wait()',h.context);
        answer(h,h.requests.at(-1),modelSnapshot('model-native','completion',native));
        await assert.rejects(pending,/EVAL_MODEL_PROTOCOL/);
    }
});
test('mixed model and agent waits preserve input order and consume one shared deadline', async () => {
    let clock = 1000;
    const h = host({Date:class extends Date { static now() { return clock; } }});
    await launchAgent(h,'agentJob','agent-native');
    await launchModel(h,"completion('work')",'completion');
    const pending = vm.runInContext('wait([model,agentJob],{timeout:10,raiseErrors:false})',h.context);
    assert.equal(h.requests.at(-1).args.args.timeoutMs,10000);
    clock = 7500;
    answer(h,h.requests.at(-1),modelSnapshot('model-native','completion',
        modelResult('completion',null,true,'PI_COMPLETION_PROVIDER'),'failed'));
    await new Promise(resolve=>setImmediate(resolve));
    assert.deepEqual(h.requests.at(-1).args,{name:'jobs',args:{action:'wait',jobId:'agent-native',timeoutMs:3500}});
    answer(h,h.requests.at(-1),jobSnapshot('agent-native'));
    const values = await pending;
    assert.equal(values[0].details.error,'PI_COMPLETION_PROVIDER');
    assert.equal(values[1].details.results[0].data.count,7);
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
    const pools = new Map();
    const operations = [];
    const snapshot = pool => {
        const counts = {queued:0,running:0,completed:0,failed:0,cancelled:0};
        for (const item of pool.items) counts[item.status]++;
        return {schema:'pi.subagent.pool.v1',poolId:pool.id,jobId:pool.jobId,agent:pool.agent,
            status:pool.status,sealed:pool.sealed,concurrency:pool.concurrency,
            admittedItems:pool.items.length,queuedItems:counts.queued,runningItems:counts.running,
            completedItems:counts.completed,failedItems:counts.failed+counts.cancelled,remainingMs:900000,
            items:pool.items.map(item=>({id:item.id,status:item.status,summary:item.result ? `item ${item.id}` : '',
                resultRef:{poolId:pool.id,itemId:item.id}}))};
    };
    const complete = (pool,id,result=turnResult(id)) => Object.assign(pool.items[id-1],{
        status:result.isError ? 'failed' : 'completed',result,
    });
    const apply = request => {
        const args = request.args.args;
        if (request.args.name === 'jobs') {
            operations.push(`jobs:${args.action}`);
            const pool = Array.from(pools.values()).find(pool=>pool.jobId===args.jobId);
            assert.ok(pool,'aggregate lookup must use the single carrier');
            if (!pool.sealed) return jobSnapshot(pool.jobId,'running');
            for (const item of pool.items) if (item.result === null) complete(pool,item.id);
            pool.status = pool.items.some(item=>item.status==='failed') ? 'failed' : 'completed';
            return jobSnapshot(pool.jobId,'exited',{content:[{type:'text',text:'aggregate completed'}],
                details:snapshot(pool),isError:pool.status==='failed'});
        }
        assert.equal(request.args.name,'subagent');
        operations.push(args.operation);
        let pool;
        if (args.operation === 'pool_start') {
            const id = `pool-native-${pools.size + 1}`;
            pool = {id,jobId:`carrier-${id}`,agent:args.agent,concurrency:args.concurrency,
                status:'open',sealed:false,items:[]};
            pools.set(id,pool);
        } else {
            pool = pools.get(args.poolId);
            assert.ok(pool,'operation must reference an acknowledged native pool');
        }
        const admittedItemIds = [];
        if (args.operation === 'pool_start' || args.operation === 'pool_push') {
            assert.ok(!pool.sealed);
            for (const task of args.tasks) {
                const id = pool.items.length+1;
                pool.items.push({id,task,status:id<=pool.concurrency ? 'running' : 'queued',result:null});
                admittedItemIds.push(id);
            }
        } else if (args.operation === 'pool_finish') {
            pool.sealed = true;
            pool.status = pool.items.some(item=>item.result===null) ? 'draining' : 'completed';
        } else if (args.operation === 'pool_close') {
            pool.sealed = true;
            pool.status = 'cancelled';
            for (const item of pool.items) if (item.result === null) {
                item.status = 'cancelled';
                item.result = {content:[],details:{cancelled:true},isError:true};
            }
        } else assert.ok(['pool_status','pool_result','pool_wait'].includes(args.operation));
        const result = snapshot(pool);
        if (args.operation === 'pool_start' || args.operation === 'pool_push') result.admittedItemIds = admittedItemIds;
        if (args.itemId !== undefined) {
            const item = pool.items[args.itemId-1];
            result.itemId = item.id;
            result.item = {id:item.id,status:item.status,result:item.result};
        }
        return result;
    };
    return {pools,operations,apply,complete,snapshot,reply:request=>answer(h,request,apply(request))};
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
function runPool(h, code, reply) {
    const start = h.requests.length;
    return drive(h,vm.runInContext(code,h.context),reply,start);
}
async function createPool(h, options = {agent:'scout',concurrency:2}) {
    await vm.runInContext(`workpool(${JSON.stringify(options)}).then(value=>globalThis.pool=value)`,h.context);
}
test('workpool admits whole batches through one native carrier and retains fixed items across cells', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    assert.equal(h.requests.length,0);
    const handles = await runPool(h,"pool.push(['a','b','c','d','e']).then(items=>globalThis.items=items)",native.reply);
    assert.deepEqual(native.operations,['pool_start']);
    assert.deepEqual(h.requests[0].args.args,{agent:'scout',operation:'pool_start',concurrency:2,tasks:['a','b','c','d','e']});
    assert.deepEqual(Array.from(handles,item=>[item.poolId,item.jobId,item.itemId]),[1,2,3,4,5].map(id=>[
        'pool-native-1','carrier-pool-native-1',id,
    ]));
    assert.equal(handles[0].workerId,undefined);
    assert.equal(handles[0].cancel,undefined,'an item must not expose shared-carrier cancellation');
    assert.ok(Object.isFrozen(handles[0]));
    h.dispatch.clear();
    const nativePool = native.pools.get('pool-native-1');
    native.complete(nativePool,1);
    const followup = await runPool(h,"pool.push(['f'])",native.reply);
    assert.equal(followup[0].itemId,6);
    assert.equal(native.pools.size,1);
    assert.deepEqual(native.operations,['pool_start','pool_push']);
    const before = h.requests.length;
    const peek = await runPool(h,'pool.peek()',native.reply);
    assert.equal(h.requests.length,before+1,'the whole manifest uses one hostcall');
    assert.equal(peek.items.length,6);
    assert.equal(peek.items[0].summary,'item 1');
    assert.equal(peek.items[0].result,undefined,'manifests carry bounded summaries and result references');
    assert.equal((await runPool(h,'items[0].output()',native.reply)).details.turn,1);
    assert.equal((await runPool(h,'pool.status()',native.reply)).completedItems,1);
    const fresh = host();
    const restored = vm.runInContext("tool.result('subagent',{operation:'pool_status',poolId:'pool-native-1'})",fresh.context);
    answer(fresh,fresh.requests[0],native.apply(fresh.requests[0]));
    assert.equal((await restored).details.admittedItems,6,'native state is addressable from a fresh guest realm');
});
test('workpool validates every task and option before dispatch and admits 32 items in one hostcall', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    for (const tasks of [['valid',''],['valid',null],['valid','x\0y'],['valid','é'.repeat(32769)],['valid','\ud800']]) {
        await assert.rejects(vm.runInContext(`pool.push(${JSON.stringify(tasks)})`,h.context),/EVAL_POOL_INPUT/);
        assert.equal(h.requests.length,0);
    }
    await assert.rejects(vm.runInContext("pool.push(Array(4).fill('x'.repeat(65536)))",h.context),/encoded batch exceeds 256 KiB/);
    assert.equal(vm.runInContext('pool.closed || pool.unknownAdmission',h.context),false);
    assert.equal(h.requests.length,0);
    const handles = await runPool(h,'pool.push(Array.from({length:32},(_,n)=>`task ${n}`))',native.reply);
    assert.equal(handles.length,32);
    assert.equal(h.requests.length,1);
    await assert.rejects(vm.runInContext("pool.push(['over capacity'])",h.context),/EVAL_POOL_CAPACITY/);
    for (const options of [{concurrency:0},{concurrency:9},{concurrency:1.5},{concurrency:true},
        {isolation:'worktree'},{timeoutSeconds:0},{scope:'all'},{keepAlive:false},{outputSchema:{}},{cwd:null}]) {
        await assert.rejects(vm.runInContext(`workpool(${JSON.stringify(options)})`,h.context),/EVAL_POOL_INPUT/);
    }
    assert.equal(h.requests.length,1);
});
test('workpool forwards native launch options while keeping its display name local', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h,{agent:'scout',concurrency:3,name:'review',cwd:'.',scope:'project',isolation:'none',timeoutSeconds:120});
    await runPool(h,"pool.push(['inspect'])",native.reply);
    assert.deepEqual(h.requests[0].args.args,{agent:'scout',cwd:'.',scope:'project',isolation:'none',timeoutSeconds:120,
        operation:'pool_start',concurrency:3,tasks:['inspect']});
    assert.equal(vm.runInContext('pool.name',h.context),'review');
});
test('a lost batch acknowledgment preserves handles and reconciles native admission without replay', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    await runPool(h,"pool.push(['a','b'])",native.reply);
    const lost = request => {
        native.apply(request);
        h.dispatch.dispatch(request.id,false,false,'acknowledgment lost');
    };
    await assert.rejects(runPool(h,"pool.push(['c','d'])",lost),error=>{
        assert.match(error.message,/EVAL_POOL_ADMISSION/);
        assert.equal(error.admitted.length,2);
        assert.equal(error.requested,2);
        return true;
    });
    assert.equal(vm.runInContext('pool.items.length',h.context),2);
    assert.equal(vm.runInContext('pool.unknownAdmission',h.context),true);
    await assert.rejects(vm.runInContext("pool.push(['retry'])",h.context),/EVAL_POOL_CLOSED/);
    const peek = await runPool(h,'pool.peek()',native.reply);
    assert.equal(peek.items.length,4);
    assert.equal(peek.closed,true);
    assert.equal(peek.unknownAdmission,false);
    assert.equal(vm.runInContext('pool.items[3].itemId',h.context),4);
    assert.deepEqual(native.operations,['pool_start','pool_push','pool_status']);
    assert.equal((await runPool(h,'pool.close()',native.reply)).allClosed,true);
});
test('finish seals admission once and returns a retained aggregate job that drains every item', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h,{agent:'scout',concurrency:1});
    const settled = await runPool(h,"Promise.allSettled([pool.push(['a','b']),pool.finish(),pool.push(['too late'])])",native.reply);
    assert.equal(settled[0].value.length,2);
    assert.equal(settled[1].value.id,'carrier-pool-native-1');
    assert.match(settled[2].reason.message,/EVAL_POOL_CLOSED/);
    assert.deepEqual(native.operations,['pool_start','pool_finish']);
    assert.equal((await vm.runInContext('pool.finish()',h.context)).id,'carrier-pool-native-1');
    const result = await runPool(h,'pool.wait(10)',native.reply);
    assert.equal(result.details.status,'completed');
    assert.deepEqual(Array.from(result.details.items,item=>item.id),[1,2]);
    assert.deepEqual(native.operations,['pool_start','pool_finish','jobs:wait']);
    assert.equal((await runPool(h,'pool.output()',native.reply)).details.completedItems,2);
    await assert.rejects(vm.runInContext('pool.wait(-1)',h.context),/EVAL_AGENT_INPUT/);
    const empty = host(); const emptyNative = poolHost(empty); await createPool(empty);
    assert.equal((await runPool(empty,'pool.finish()',emptyNative.reply)).id,'carrier-pool-native-1');
    assert.deepEqual(emptyNative.operations,['pool_start','pool_finish']);
    assert.deepEqual(empty.requests[0].args.args.tasks,[]);
});
test('mixed waits preserve order, one timeout, pinned worker turns and failed native pool results', async () => {
    const h = host(); const native = poolHost(h);
    await launchAgent(h,'job','job-once');
    await launchWorker(h);
    await createPool(h);
    await runPool(h,"pool.push(['a']).then(items=>globalThis.item=items[0])",native.reply);
    const failed = {content:[{type:'text',text:'item failed'}],details:{item:1},isError:true};
    native.complete(native.pools.get('pool-native-1'),1,failed);
    const waiting = vm.runInContext('wait([job,item,worker],{timeout:10,raiseErrors:false})',h.context);
    const jobRequest = h.requests.at(-1);
    const sending = vm.runInContext("worker.send('next')",h.context);
    answer(h,h.requests.at(-1),workerSnapshot('worker-native',2,'queued'));
    await sending;
    const start = h.requests.length;
    answer(h,jobRequest,jobSnapshot('job-once'));
    const results = await drive(h,waiting,request=>{
        assert.ok(request.args.args.timeoutMs<=10000);
        if (request.args.args.operation==='pool_wait') native.reply(request);
        else {
            assert.equal(request.args.args.turnId,1);
            answer(h,request,workerSnapshot('worker-native',1,'completed',turnResult(1)));
        }
    },start);
    assert.equal(results[0].details.results[0].data.count,7);
    assert.equal(results[1].isError,true);
    assert.equal(results[2].details.turn,1);
    assert.equal(vm.runInContext('worker.turnId',h.context),2);
});
test('pool item waits retain pending state and validate current native identity and terminal output', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    await runPool(h,"pool.push(['a']).then(items=>globalThis.item=items[0])",native.reply);
    await assert.rejects(runPool(h,'item.wait(0)',native.reply),/EVAL_AGENT_TIMEOUT/);
    assert.deepEqual(native.operations,['pool_start','pool_wait']);
    const pool = native.pools.get('pool-native-1');
    native.complete(pool,1);
    const good = {...native.snapshot(pool),itemId:1,item:{id:1,status:'completed',result:turnResult(1)}};
    for (const bad of [{...good,poolId:'other'},{...good,jobId:'other'},{...good,agent:'other'},
        {...good,itemId:2},{...good,item:{id:1,status:'completed',result:null}},
        {...good,items:[{...good.items[0],resultRef:{poolId:pool.id,itemId:2}}]},
        {...good,completedItems:0}]) {
        await assert.rejects(runPool(h,'item.status()',request=>answer(h,request,bad)),/EVAL_POOL_PROTOCOL/);
    }
    await assert.rejects(runPool(h,'item.output()',request=>{
        h.dispatch.dispatch(request.id,false,false,'PI_POOL_OWNER_CHANGED');
    }),/PI_POOL_OWNER_CHANGED/);
});
test('workpool serializes cancellation and refuses later admission without native dispatch', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    const settled = await runPool(h,"Promise.allSettled([pool.push(['a','b']),pool.close(),pool.push(['too late'])])",native.reply);
    assert.equal(settled[0].status,'fulfilled');
    assert.equal(settled[1].value.allClosed,true);
    assert.equal(settled[1].value.status,'cancelled');
    assert.match(settled[2].reason.message,/EVAL_POOL_CLOSED/);
    assert.deepEqual(native.operations,['pool_start','pool_close']);
    assert.equal((await vm.runInContext('pool.close()',h.context)).allClosed,true);
    assert.equal(h.requests.length,2,'acknowledged closes are not replayed');
    const empty = host(); await createPool(empty);
    assert.equal((await vm.runInContext('pool.close()',empty.context)).allClosed,true);
    assert.equal(empty.requests.length,0);
});
test('an interrupted pool command can cancel its known carrier without a stuck guest queue', async () => {
    const h = host(); const native = poolHost(h);
    await createPool(h);
    await runPool(h,"pool.push(['a'])",native.reply);
    vm.runInContext("pool.push(['b'])",h.context);
    await new Promise(resolve=>setImmediate(resolve));
    assert.equal(h.requests[1].args.args.operation,'pool_push');
    native.apply(h.requests[1]);
    h.dispatch.clear();
    const closed = await runPool(h,'pool.close()',native.reply);
    assert.equal(closed.allClosed,true);
    assert.equal(closed.items.length,2);
    assert.equal(closed.interrupted,true);
    assert.equal(vm.runInContext('pool.items.length',h.context),2);
    assert.deepEqual(native.operations,['pool_start','pool_push','pool_close']);
});
test('workpool reports uncertainty after a lost start acknowledgment without retry or false cleanup', async () => {
    const h = host(); await createPool(h);
    await assert.rejects(runPool(h,"pool.push(['first'])",request=>{
        h.dispatch.dispatch(request.id,false,false,'acknowledgment lost');
    }),/EVAL_POOL_ADMISSION/);
    const closed = await vm.runInContext('pool.close()',h.context);
    assert.equal(closed.allKnownClosed,true);
    assert.equal(closed.unknownAdmission,true);
    assert.equal(closed.allClosed,false);
    await assert.rejects(vm.runInContext('pool.finish()',h.context),/EVAL_POOL_CLOSED/);
    assert.equal(h.requests.length,1);
});
