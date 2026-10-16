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
