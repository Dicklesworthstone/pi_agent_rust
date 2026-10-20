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
    for (const option of ['async','task','op','model','__proto__']) {
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
