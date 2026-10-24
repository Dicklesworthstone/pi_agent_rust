'use strict';
// Production browser prelude over a controlled native transport, not CDP or
// Rust acceptance. The integration suite covers the actual QuickJS host path.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname,'../src/eval/js_kernel/browser.js'),'utf8');
const plain = value => JSON.parse(JSON.stringify(value));
const result = (details = {}, content = [], isError = false) => ({
    schema:'pi.eval.tool_result.v1',content,details,isError,
});
function host() {
    const calls = [];
    const h = {calls, respond: async args => result({action:args.action,tab:args.tab})};
    const context = vm.createContext({tool: {result:async (name,args) => {
        assert.equal(name,'browser');
        const wire = plain(args);
        calls.push(wire);
        return h.respond(wire);
    }}});
    vm.runInContext(source,context);
    h.run = code => vm.runInContext(`(async () => { ${code} })()`,context);
    return h;
}

test('named tabs open once and retain the complete native admission output', async () => {
    const h = host();
    const opened = result({tab:'app',url:'https://example.test'},[{type:'text',text:'loaded'}]);
    h.respond = async () => opened;
    const view = plain(await h.run("globalThis.tab = await browser.open({name:'app',url:'https://example.test',timeout_ms:5000}); return {name:tab.name,opened:tab.opened};"));
    assert.deepEqual(view,{name:'app',opened});
    assert.deepEqual(h.calls,[{timeout_ms:5000,url:'https://example.test',action:'open',tab:'app'}]);
});

test('semantic locators dispatch literal names and trusted-native input requests', async () => {
    const h = host();
    await h.run(`const tab = browser.tab('app');
        await tab.getByLabel('Email').fill('person@example.test');
        await tab.getByRole('button', 'Save "draft"').click({wait_after:{selector:'#status',text:'Saved'}});
        await tab.getByText('literal \u96ea').hover();
        await tab.getByTestId('result').waitFor({state:'attached'});`);
    assert.deepEqual(h.calls,[
        {action:'fill',tab:'app',selector:'label/Email',text:'person@example.test'},
        {action:'click',tab:'app',selector:'role/button[name="Save \\"draft\\"" exact]',wait_after:{selector:'#status',text:'Saved'}},
        {action:'hover',tab:'app',selector:'text/literal 雪'},
        {action:'wait_for',tab:'app',selector:'testid/result',state:'attached'},
    ]);
});

test('frame scopes and snapshot references remain explicit on every invocation', async () => {
    const h = host();
    await h.run("globalThis.frame = browser.tab('app').frame('native-frame'); await frame.snapshot(); await frame.ref('@e3').press('Enter'); await frame.call('scroll',{selector:'.list',delta_y:100});");
    assert.deepEqual(h.calls,[
        {action:'snapshot',tab:'app',frame:'native-frame'},
        {action:'press',tab:'app',frame:'native-frame',selector:'@e3',key:'Enter'},
        {action:'scroll',tab:'app',frame:'native-frame',selector:'.list',delta_y:100},
    ]);
});

test('a scripted decision can consume snapshot evidence before native action without another model turn', async () => {
    const h = host();
    h.respond = async args => result(args.action === 'snapshot'
        ? {snapshot:{elements:[{text:'Save',ref_id:'@e7'},{text:'Cancel',ref_id:'@e8'}]}}
        : {dispatched:true});
    await h.run(`const tab = browser.tab('app'); const seen = await tab.snapshot();
        const save = seen.details.snapshot.elements.find(element => element.text === 'Save');
        if (!save) throw new Error('Save control missing');
        await tab.ref(save.ref_id).click();`);
    assert.deepEqual(h.calls.map(call => [call.action,call.selector]),[['snapshot',undefined],['click','@e7']]);
});

test('native batch retains bindings, assertions, scoped calls and one outer budget', async () => {
    const h = host();
    await h.run(`await browser.tab('board').batch([
        {call:{action:'snapshot'},expect:{pointer:'/snapshot/elements/0/text',equals:'Move'}},
        {call:{action:'drag',target_selector:'testid/done'},bind:{selector:{step:0,pointer:'/snapshot/elements/0/ref_id'}}}
    ],{timeout_ms:15000});`);
    assert.equal(h.calls.length,1);
    assert.equal(h.calls[0].action,'batch');
    assert.equal(h.calls[0].timeout_ms,15000);
    assert.equal(h.calls[0].steps[1].call.tab,'board');
    assert.deepEqual(h.calls[0].steps[1].bind,{selector:{step:0,pointer:'/snapshot/elements/0/ref_id'}});
});

test('an invalid later batch step cannot send a valid prefix', async () => {
    const h = host();
    for (const steps of ["[{call:{action:'click',selector:'#x'}},{}]",
        "[{call:{action:'snapshot'}},{call:{action:'click',tab:'other'}}]",
        "[{call:{action:'snapshot'}},,]", "[]"]) {
        await assert.rejects(h.run(`return await browser.tab('a').batch(${steps});`),/INPUT/);
    }
    assert.equal(h.calls.length,0);
});

test('failed actions throw with complete native partial evidence and do not replay', async () => {
    const h = host();
    const failure = result({completedSteps:1,steps:[{ok:true},{ok:false}]},[{type:'text',text:'postcondition failed'}],true);
    h.respond = async () => failure;
    await assert.rejects(h.run("await browser.tab('a').locator('#save').click(); await browser.tab('a').reload();"),error => {
        assert.equal(error.name,'BrowserActionError');
        assert.equal(error.action,'click');
        assert.deepEqual(plain(error.result),failure);
        return true;
    });
    assert.equal(h.calls.length,1);
});

test('native batch failure preserves completed-step evidence on the thrown error', async () => {
    const h = host();
    const failure = result({completedSteps:1,failedStep:1},[{type:'text',text:'assertion mismatch'}],true);
    h.respond = async () => failure;
    await assert.rejects(h.run("return await browser.tab('a').batch([{call:{action:'snapshot'}},{call:{action:'click',selector:'#x'}}]);"),error => {
        assert.equal(error.action,'batch'); assert.deepEqual(plain(error.result),failure); return true;
    });
    assert.equal(h.calls.length,1);
});

test('retained handles use the current policy instead of retaining approval or cached results', async () => {
    const h = host();
    await h.run("globalThis.saved = browser.tab('a').locator('#save'); await saved.click();");
    h.respond = async () => { throw new Error('current host denied browser'); };
    await assert.rejects(h.run('return await saved.click();'),/current host denied/);
    assert.equal(h.calls.length,2);
});

test('native result transport is captured rather than replaceable through public tool.result', async () => {
    const h = host();
    await h.run("tool.result = async () => { throw new Error('replacement'); }; await browser.tab('a').snapshot();");
    assert.equal(h.calls.length,1);
});

test('tab, action, frame and input arguments cannot be overwritten by options', async () => {
    const h = host();
    for (const call of [
        "browser.open({name:'a',url:'https://example.test',action:'stop'})",
        "browser.tab('a').snapshot({tab:'b'})", "browser.tab('a').frame('f').snapshot({frame:'g'})",
        "browser.tab('a').locator('#x').fill('one',{text:'two'})",
        "browser.tab('a').locator('#x').click({selector:'#y'})",
        "browser.tab('a').goto('https://example.test',{url:'https://other.test'})",
    ]) await assert.rejects(h.run(`return await ${call};`),/INPUT/);
    assert.equal(h.calls.length,0);
});

test('drag targets must be branded locators in the same tab and frame', async () => {
    const h = host();
    await h.run("const t = browser.tab('a'); await t.locator('#source').dragTo(t.locator('#target'),{drag_steps:12});");
    assert.deepEqual(h.calls[0],{action:'drag',tab:'a',selector:'#source',target_selector:'#target',drag_steps:12});
    h.calls.length = 0;
    for (const target of ["browser.tab('b').locator('#target')", "browser.tab('a').frame('f').locator('#target')", "{selector:'#target'}"]) {
        await assert.rejects(h.run(`return await browser.tab('a').locator('#source').dragTo(${target});`),/INPUT/);
    }
    assert.equal(h.calls.length,0);
});

test('form values and file paths stay literal and full screenshot content survives', async () => {
    const h = host();
    const image = result({width:3,height:2},[{type:'image',data:'bytes',mimeType:'image/png'}]);
    h.respond = async args => args.action === 'screenshot' ? image : result();
    await h.run("const f = browser.tab('a').locator('#field'); await f.fill(''); await f.type('literal {previous}'); await f.upload(['reports/a.csv']); await f.download('downloads/report.csv');");
    assert.deepEqual(h.calls.map(call => call.action),['fill','type','upload','download']);
    assert.equal(h.calls[0].text,'');
    assert.equal(h.calls[1].text,'literal {previous}');
    assert.deepEqual(plain(await h.run("return await browser.tab('a').screenshot();")),image);
});

test('queued argument snapshots are independent of caller mutations', async () => {
    const h = host();
    await h.run("const options = {wait_after:{selector:'#status',text:'saved'}}; const p = browser.tab('a').locator('#x').click(options); options.wait_after.text = 'changed'; await p;");
    assert.equal(h.calls[0].wait_after.text,'saved');
});

test('unsupported JSON types, getters, cycles and sparse arrays fail before dispatch', async () => {
    const h = host();
    for (const value of ['undefined','NaN','Infinity','1n','() => 1','new Date()', '[1,,2]', 'Symbol()']) {
        await assert.rejects(h.run(`return await browser.tab('a').call('evaluate',{extra:${value}});`),/INPUT/);
    }
    await assert.rejects(h.run("const x = {}; x.self=x; return await browser.tab('a').call('evaluate',x);"),/INPUT/);
    await assert.rejects(h.run("globalThis.reads=0; const x = {get timeout_ms(){reads++;return 1;}}; return await browser.tab('a').snapshot(x);"),/INPUT/);
    assert.equal(await h.run('return reads;'),0);
    assert.equal(h.calls.length,0);
});

test('own and inherited toJSON hooks never rewrite a protected native action', async () => {
    const h = host();
    await h.run("Object.prototype.toJSON = function(){throw new Error('untrusted serialization');}; Array.prototype.toJSON = Object.prototype.toJSON; await browser.tab('a').locator('#x').upload(['a.csv']);");
    assert.deepEqual(h.calls,[{selector:'#x',files:['a.csv'],action:'upload',tab:'a'}]);
    await assert.rejects(h.run("return await browser.tab('a').snapshot({toJSON(){return {action:'stop'};}});"),/INPUT/);
    assert.equal(h.calls.length,1);
});

test('malformed host replies fail rather than producing a plausible empty snapshot', async () => {
    for (const malformed of [null,{},{...result(),details:undefined},{...result(),isError:undefined},{...result(),content:null}]) {
        if (malformed && Object.hasOwn(malformed,'details') && malformed.details === undefined) delete malformed.details;
        const h = host(); h.respond = async () => malformed;
        await assert.rejects(h.run("return await browser.tab('a').snapshot();"),/PROTOCOL/);
        assert.equal(h.calls.length,1);
    }
});

test('browser and handles are frozen, and ordinary native lifecycle actions are explicit', async () => {
    const h = host();
    assert.deepEqual(plain(await h.run("return [Object.isFrozen(browser),Object.isFrozen(browser.tab('a')),Object.isFrozen(browser.tab('a').locator('#x'))];")),[true,true,true]);
    await h.run('await browser.status(); await browser.tabs(); await browser.start(); await browser.stop();');
    assert.deepEqual(h.calls.map(call => call.action),['status','list_tabs','start','stop']);
});

test('page evaluation requires explicit source and never serializes a closure', async () => {
    const h = host();
    await assert.rejects(h.run("return await browser.tab('a').evaluate(() => secret);"),/INPUT/);
    await h.run("await browser.tab('a').evaluate('({title:document.title})');");
    assert.deepEqual(h.calls,[{script:'({title:document.title})',action:'evaluate',tab:'a'}]);
});

test('depth and node budgets refuse pathological data before host admission', async () => {
    const h = host();
    await assert.rejects(h.run("let v = {}; for(let i=0;i<34;i++)v={next:v}; await browser.tab('a').snapshot({v});"),/limits/);
    await assert.rejects(h.run("await browser.tab('a').snapshot({v:Array(16385).fill(1)});"),/limits/);
    await assert.rejects(h.run("await browser.tab('a').snapshot({v:'x'.repeat(262145)});"),/budget/);
    assert.equal(h.calls.length,0);
});
