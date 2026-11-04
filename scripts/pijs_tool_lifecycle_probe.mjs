/**
 * Supplementary, network-free diagnostic for PiJS tool progress and cancellation.
 * Requires Node >= 24 and --experimental-vm-modules:
 *
 *   node --experimental-vm-modules scripts/pijs_tool_lifecycle_probe.mjs
 *   node --experimental-vm-modules scripts/pijs_tool_lifecycle_probe.mjs --source /path/to/prior/extensions_js.rs
 *
 * The unchanged repository subagent example is pinned below. A sparse checkout
 * can supply its exact index.ts with --extension /path/to/subagent/index.ts.
 * Source provenance: Dicklesworthstone/pi_agent_rust commit
 * 152c13d3e5ba6c38e742bfd303eaf52783995d23,
 * tests/ext_conformance/artifacts/subagent/index.ts,
 * Git blob 1c43cbfc2ecdb817e332170763cfc0b1f0290074.
 *
 * This runs the verbatim embedded JS registration, dispatch, invocation lifetime,
 * privileged-export guard, AbortController and TypeBox source in V8. The example
 * is only type-stripped in memory. Child processes, agent discovery, StringEnum,
 * host-secret validation, update sinks, timers, Event/EventTarget and UI are
 * controlled fixtures. Other imported modules are unused stubs. Small tool callbacks
 * exercise lifecycle edges which the real example cannot intentionally cause.
 * The example's replacement snapshots and distinct parallel final summary are
 * observed at the bridge sink; terminal rendering and UI delivery are not run.
 *
 * This does NOT execute Rust/QuickJS, native mailbox/backpressure, deadline/drop
 * handling, native capability/ownership enforcement, real child termination or
 * Windows. In particular it does not prove cancellation of context-mode, whose
 * published callback ignores Pi's signal. V8 vm is not a security boundary.
 * DSR remains the authoritative gate: dsr quality --tool pi_agent_rust.
 */

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import EventEmitter from 'node:events';
import { readFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import vm from 'node:vm';

const { values } = parseArgs({ options: {
  source: { type: 'string' }, extension: { type: 'string' }, help: { type: 'boolean' },
} });
if (values.help) {
  console.log('Usage: node --experimental-vm-modules scripts/pijs_tool_lifecycle_probe.mjs [--source /path/to/extensions_js.rs] [--extension /path/to/subagent/index.ts]');
  process.exit(0);
}
assert.equal(typeof vm.SourceTextModule, 'function', 'Run with --experimental-vm-modules.');
const sourcePath = values.source ?? fileURLToPath(new URL('../src/extensions_js.rs', import.meta.url));
const extensionPath = values.extension ?? fileURLToPath(new URL('../tests/ext_conformance/artifacts/subagent/index.ts', import.meta.url));
const rust = readFileSync(sourcePath, 'utf8');
const extension = readFileSync(extensionPath, 'utf8');
const sha256 = source => createHash('sha256').update(source).digest('hex');
assert.equal(sha256(extension), '9f0ad03051954af239b9bf299ef3dbb1696ebebe0b9b4bcc1c22b9d2489066b2',
  'The checked-in subagent fixture must be unchanged; see its pinned provenance above.');

function slice(startMarker, endMarker, optional = false) {
  const start = rust.indexOf(startMarker);
  if (start < 0 && optional) return '';
  assert(start >= 0 && rust.indexOf(startMarker, start + startMarker.length) < 0,
    `Unique source marker: ${startMarker}`);
  const end = rust.indexOf(endMarker, start + startMarker.length);
  assert(end > start, `Source end marker: ${endMarker}`);
  return rust.slice(start, end);
}

const blocks = {
  primordials: slice('const __pi_original_Map = Map;', 'const __pi_secret_matches ='),
  secret: slice('const __pi_secret_matches =', '// ============================================================================\n// Console global'),
  registry: slice('let __pi_current_extension_id = __pi_fixed_extension_id;', 'function __pi_runtime_registry_snapshot()'),
  ownerScope: slice('function __pi_current_extension_or_throw()', '// Pattern 5 (bd-k5q5.8.6)'),
  registration: slice('function __pi_register_tool(spec)', 'function __pi_register_command('),
  context: slice('function __pi_make_extension_ctx(ctx_payload)', '// Capture the JSON reader and native snapshotter'),
  execution: slice('function __pi_validate_tool_input(schema, input)', 'async function __pi_execute_command('),
  abort: slice('    const __pi_fetch_abort_error = () => {', '    const __pi_fetch = async ('),
  lifetime: slice('const __pi_tool_invocation_runtime = (() => {', 'const __pi_export_privileged =', true),
  exportGuard: slice('const __pi_export_privileged =', "__pi_export_privileged(\n    '__pi_reset_extension_runtime_state'"),
};
const exportList = slice('for (const [name, fn] of Object.entries({', "Object.defineProperty(globalThis, '__pi_get_current_extension_id'");
const protectedNames = new Set([...exportList.matchAll(/^\s+(__pi_\w+),?$/gm)].map(match => match[1]));
const controls = ['__pi_execute_tool', '__pi_cancel_tool_invocation', '__pi_finish_tool_invocation'];
const availableControls = controls.filter(name => protectedNames.has(name));

const typeboxMarker = '"@sinclair/typebox".to_string(),\n        compressed_js_literal!(';
const typeboxStart = rust.indexOf(typeboxMarker);
assert(typeboxStart >= 0, 'Embedded TypeBox marker');
const literal = /^\s*r(#+)"/.exec(rust.slice(typeboxStart + typeboxMarker.length));
assert(literal, 'Embedded TypeBox raw literal');
const typeboxBody = typeboxStart + typeboxMarker.length + literal[0].length;
const typeboxEnd = rust.indexOf('"' + literal[1], typeboxBody);
assert(typeboxEnd > typeboxBody, 'Embedded TypeBox terminator');
const typebox = rust.slice(typeboxBody, typeboxEnd);
const strippedExtension = stripTypeScriptTypes(extension, { mode: 'strip' });
console.log('# ' + JSON.stringify({
  runtime: process.version, platform: process.platform, source: sourcePath,
  fixture: 'repository subagent example at 152c13d', fixtureSha256: sha256(extension),
  embeddedSha256: Object.fromEntries(Object.entries({ ...blocks, exportList, typebox }).map(([key, source]) => [key, sha256(source)])),
  verification: 'V8 source diagnostic with controlled processes and native callbacks; not Rust/QuickJS or Windows execution',
}));

const turn = () => new Promise(resolve => setImmediate(resolve));
const content = text => ({ content: [{ type: 'text', text }], details: { fixture: true } });

async function fixture({ loadExample = false, fixedOwner = true } = {}) {
  const owner = 'subagent-fixture';
  const secret = Object.freeze({ fixtureSecret: true });
  const children = [], timers = new Map();
  let nextTimer = 0, nextToken = 0;
  const context = vm.createContext({
    Event, EventTarget,
    __pi_bridge_secret_matches_native: candidate => candidate === secret,
    __pi_owner_extension_id_native: () => fixedOwner ? owner : null,
    __pi_make_extension_ui: () => ({}),
    setTimeout(callback, ms) { const id = ++nextTimer; timers.set(id, { callback, ms }); return id; },
    clearTimeout(id) { timers.delete(id); },
  });
  const api = vm.runInContext(`(() => {
    ${Object.values(blocks).join('\n')}
    __pi_current_extension_id = ${JSON.stringify(owner)};
    __pi_extensions.set(${JSON.stringify(owner)}, { id: ${JSON.stringify(owner)}, tools: new Map() });
    ${availableControls.map(name => `__pi_export_privileged('${name}', ${name});`).join('\n')}
    return {
      registerTool: __pi_register_tool,
      count: () => typeof __pi_tool_invocation_runtime === 'undefined' ? undefined : __pi_tool_invocation_runtime.count(),
      resetInvocations: () => { if (typeof __pi_tool_invocation_runtime === 'undefined') throw new Error('Missing invocation runtime'); __pi_tool_invocation_runtime.reset(); },
      injectCurrentOwner: owner => { __pi_current_extension_id = owner; },
    };
  })()`, context, { timeout: 1000 });

  function spawn(command, args, options) {
    assert.equal(command, 'pi');
    assert(args.includes('--mode') && args.includes('json'));
    assert.equal(options.shell, false);
    const child = new EventEmitter();
    Object.assign(child, {
      stdout: new EventEmitter(), stderr: new EventEmitter(),
      killed: false, kills: [], closed: false,
      kill(signal) { this.kills.push(signal); this.killed = true; this.onKill?.(); return true; },
      send(text) {
        this.stdout.emit('data', Buffer.from(JSON.stringify({ type: 'message_end', message: {
          role: 'assistant', content: [{ type: 'text', text }],
        } }) + '\n'));
      },
      close(code = 0) { if (!this.closed) { this.closed = true; this.emit('close', code); } },
    });
    children.push(child);
    return child;
  }

  if (loadExample) {
    const unused = () => { throw new Error('Unexpected unused example dependency'); };
    const modules = new Map();
    const synthetic = (name, exports) => {
      const module = new vm.SyntheticModule(Object.keys(exports), function () {
        for (const [key, value] of Object.entries(exports)) this.setExport(key, value);
      }, { context, identifier: name });
      modules.set(name, module);
    };
    synthetic('node:child_process', { spawn });
    synthetic('node:fs', { mkdtempSync: unused, writeFileSync: unused, unlinkSync: unused, rmdirSync: unused });
    synthetic('node:os', { tmpdir: unused });
    synthetic('node:path', { join: unused });
    synthetic('@mariozechner/pi-ai', { StringEnum: (items, options) => ({ type: 'string', enum: items, ...options }) });
    synthetic('@mariozechner/pi-coding-agent', { getMarkdownTheme: unused });
    synthetic('@mariozechner/pi-tui', { Container: unused, Markdown: unused, Spacer: unused, Text: unused });
    synthetic('./agents.js', { discoverAgents: () => ({
      agents: [{ name: 'probe', source: 'user', systemPrompt: '' }], projectAgentsDir: null,
    }) });
    modules.set('@sinclair/typebox', new vm.SourceTextModule(typebox, { context, identifier: 'actual-embedded-typebox' }));
    const module = new vm.SourceTextModule(strippedExtension, { context, identifier: extensionPath });
    await module.link(name => { assert(modules.has(name), `Unexpected example import: ${name}`); return modules.get(name); });
    await module.evaluate({ timeout: 1000 });
    module.namespace.default({ registerTool: api.registerTool });
  }

  function control(name, token, key = secret) {
    assert.equal(typeof context[name], 'function', `Missing protected bridge control: ${name}`);
    return context[name](key, token);
  }
  return {
    context, api, children, timers, secret, owner,
    register(name, execute) { api.registerTool({ name, parameters: { type: 'object' }, execute }); },
    start(name = 'subagent', input = { agent: 'probe', task: 'fixture task' }, options = {}) {
      const token = options.token ?? `native-fixture-${++nextToken}`;
      const updates = [], encodedUpdates = [];
      const sink = options.sink === null ? null : encoded => {
        assert.equal(typeof encoded, 'string', 'Native sink receives one JSON string');
        encodedUpdates.push(encoded);
        updates.push(JSON.parse(encoded));
        options.sink?.(encoded);
      };
      const promise = context.__pi_execute_tool(secret, name, options.callId ?? 'public-call', input,
        { cwd: '/fixture/project', hasUI: false }, token, sink);
      let settled = false;
      const outcome = promise.then(value => { settled = true; return { value }; }, error => { settled = true; return { error }; });
      return { token, updates, encodedUpdates, promise, outcome, get settled() { return settled; } };
    },
    cancel: token => control('__pi_cancel_tool_invocation', token),
    finish: token => control('__pi_finish_tool_invocation', token),
    control,
  };
}

test('unchanged subagent callback streams detached snapshots before its final result', async () => {
  const f = await fixture({ loadExample: true });
  const op = f.start();
  const child = f.children[0];
  assert(child, 'actual example started its controlled child');
  child.send('first partial');
  assert.equal(op.settled, false);
  assert.equal(op.updates.length, 1, 'onUpdate must reach the native sink before completion');
  child.send('short');
  assert.equal(op.updates.length, 2);
  assert.equal(op.updates[0].content[0].text, 'first partial');
  assert.equal(op.updates[1].content[0].text, 'short', 'updates replace earlier text rather than append to it');
  assert.equal(op.updates[0].details.results[0].messages.length, 1, 'earlier progress remains a detached snapshot');
  assert.equal(op.updates[1].details.results[0].messages.length, 2);
  child.close();
  assert.equal((await op.promise).content[0].text, 'short');
  child.send('late partial');
  assert.equal(op.updates.length, 2, 'held example callback cannot publish after completion');
  assert.equal(f.cancel(op.token), false);
  assert.equal(f.api.count(), 0);
});

test('unchanged parallel subagent reports detached progress and a distinct final summary', async () => {
  const f = await fixture({ loadExample: true });
  const op = f.start('subagent', { tasks: [
    { agent: 'probe', task: 'first task' },
    { agent: 'probe', task: 'second task' },
  ] });
  assert.equal(f.children.length, 2, 'the real example starts both controlled tasks');
  const [first, second] = f.children;
  first.send('first result');
  assert.equal(op.settled, false);
  assert.equal(op.updates.length, 1);
  assert.equal(op.updates[0].content[0].text, 'Parallel: 1/2 done, 1 running...');
  assert.equal(op.updates[0].details.mode, 'parallel');
  assert.equal(op.updates[0].details.results.length, 2);
  assert.equal(op.updates[0].details.results[1].messages.length, 0);
  const firstSnapshot = JSON.stringify(op.updates[0]);
  first.close();
  await turn();
  assert.equal(op.settled, false, 'progress from a completed child is not the parent tool result');
  second.send('second result');
  assert.equal(op.settled, false);
  assert.equal(op.updates.at(-1).content[0].text, 'Parallel: 2/2 done, 0 running...');
  assert.equal(op.updates.at(-1).details.results[1].messages.length, 1);
  assert.equal(JSON.stringify(op.updates[0]), firstSnapshot, 'later child progress cannot mutate an earlier snapshot');
  second.close();
  const result = await op.promise;
  assert.equal(result.content[0].text,
    'Parallel: 2/2 succeeded\n\n[probe] completed: first result\n\n[probe] completed: second result');
  assert.notEqual(result.content[0].text, op.updates.at(-1).content[0].text,
    'the terminal result is authoritative and is not a replay of the last progress snapshot');
  const countAtCompletion = op.updates.length;
  first.send('late first result');
  second.send('late second result');
  assert.equal(op.updates.length, countAtCompletion);
  assert.equal(f.cancel(op.token), false);
  assert.equal(f.api.count(), 0);
});

test('unchanged subagent callback receives abort and cannot publish during its kill listener', async () => {
  const f = await fixture({ loadExample: true });
  const op = f.start();
  const child = f.children[0];
  child.send('before cancel');
  child.onKill = () => child.send('must be suppressed inside abort listener');
  assert.equal(f.cancel(op.token), true);
  assert.equal(f.cancel(op.token), false);
  assert.deepEqual(child.kills, ['SIGTERM'], 'actual example abort listener requested termination once');
  assert.equal(op.updates.length, 1);
  child.close(143);
  await assert.rejects(op.promise, /Subagent was aborted/);
  child.send('late after cancellation');
  assert.equal(op.updates.length, 1);
  assert.equal(f.api.count(), 0);
  assert.deepEqual([...f.timers.values()].map(timer => timer.ms), [5000], 'example scheduled its real escalation delay; no native timer or process was run');
});

test('same public tool-call ID does not couple independent subagent invocations', async () => {
  const f = await fixture({ loadExample: true });
  const first = f.start(), second = f.start();
  assert.notEqual(first.token, second.token);
  const [left, right] = f.children;
  assert.equal(f.cancel(first.token), true);
  assert.deepEqual(left.kills, ['SIGTERM']);
  assert.deepEqual(right.kills, []);
  right.send('other call remains active');
  assert.equal(second.updates[0].content[0].text, 'other call remains active');
  left.close(143); right.close();
  await assert.rejects(first.promise, /Subagent was aborted/);
  assert.equal((await second.promise).content[0].text, 'other call remains active');
  assert.equal(f.api.count(), 0);
});

test('execute preserves arguments and supplies a branded per-call signal even without progress', async () => {
  const f = await fixture();
  let seen;
  f.register('arguments', (...args) => { seen = args; return content('done'); });
  const op = f.start('arguments', { key: 'value' }, { sink: null, callId: 'reported-id' });
  await op.promise;
  assert.equal(seen[0], 'reported-id');
  assert.deepEqual(seen[1], { key: 'value' });
  assert(seen[2] instanceof f.context.AbortSignal, 'signal is the actual embedded AbortSignal');
  assert.equal(seen[2].aborted, false);
  assert.equal(seen[3], undefined);
  assert.equal(seen[4].cwd, '/fixture/project');
});

test('host controls require the bridge secret and immutable exported properties', async () => {
  const f = await fixture();
  for (const name of controls) {
    assert(protectedNames.has(name), `${name} must appear in the actual privileged export list`);
    assert.throws(() => f.context[name]('wrong secret', 'token'), /privileged bridge entrypoint denied/);
    assert.throws(() => f.context[name](), /privileged bridge entrypoint denied/);
    const descriptor = Object.getOwnPropertyDescriptor(f.context, name);
    assert.equal(descriptor.writable, false);
    assert.equal(descriptor.configurable, false);
  }
  assert.equal(f.cancel('unknown host token'), false);
});

test('completion and tool errors release the sink held by an extension callback', async () => {
  for (const fail of [false, true]) {
    const f = await fixture();
    let signal, update;
    f.register('retained', (_id, _input, sig, onUpdate) => {
      signal = sig; update = onUpdate; onUpdate(content('during execution'));
      if (fail) throw new Error('intentional tool failure');
      return content('done');
    });
    const op = f.start('retained', {});
    const result = await op.outcome;
    if (fail) assert.match(result.error.message, /intentional tool failure/);
    else {
      assert.equal(result.error, undefined, 'tool execution must accept its progress callback');
      assert.equal(result.value.content[0].text, 'done');
    }
    update(content('retained after completion'));
    assert.equal(op.updates.length, 1);
    assert.equal(f.cancel(op.token), false);
    assert.equal(signal.aborted, false);
    assert.equal(f.api.count(), 0);
  }
});

async function heldTool(f, name = 'held') {
  let resolve, signal, update;
  f.register(name, (_id, _input, sig, onUpdate) => {
    signal = sig; update = onUpdate;
    return new Promise(done => { resolve = done; });
  });
  return {
    start(options) {
      const operation = f.start(name, {}, options);
      assert(signal instanceof f.context.AbortSignal, 'tool must receive its invocation signal');
      assert.equal(typeof update, 'function', 'tool must receive onUpdate');
      return operation;
    },
    get signal() { return signal; },
    update(value) { assert.equal(typeof update, 'function', 'tool must receive onUpdate'); update(value); },
    complete() { resolve(content('done')); },
  };
}

test('host finish closes a held callback without triggering abort', async () => {
  const f = await fixture(), tool = await heldTool(f), op = tool.start();
  tool.update(content('before finish'));
  assert.equal(f.finish(op.token), true);
  assert.equal(f.finish(op.token), false);
  tool.update(content('after finish'));
  assert.equal(op.updates.length, 1);
  assert.equal(tool.signal.aborted, false);
  assert.equal(f.cancel(op.token), false);
  tool.complete(); await op.promise;
  assert.equal(f.api.count(), 0);
});

test('an active private token cannot be reused to replace another invocation', async () => {
  const f = await fixture(), tool = await heldTool(f);
  const first = tool.start({ token: 'host-assigned-collision' });
  const second = tool.start({ token: first.token });
  await assert.rejects(second.promise, /token is already active/);
  tool.update(content('original invocation intact'));
  assert.equal(first.updates.length, 1);
  assert.equal(second.updates.length, 0);
  tool.complete(); await first.promise;
  assert.equal(f.api.count(), 0);
});

test('invalid progress cannot prevent a later valid update or final result', async () => {
  const f = await fixture(), tool = await heldTool(f), op = tool.start();
  const cyclic = {}; cyclic.self = cyclic;
  for (const invalid of [null, [], {}, { content: {} }, { content: [], details: cyclic },
    { content: [{ type: 'text', text: 'x'.repeat(1024 * 1024) }] },
    { get content() { throw new Error('malicious getter'); } }]) tool.update(invalid);
  assert.equal(op.updates.length, 0);
  tool.update({ ...content('valid after malformed progress'), isError: true, privateField: 'omit' });
  assert.deepEqual(Object.keys(op.updates[0]).sort(), ['content', 'details']);
  assert.equal(op.updates[0].content[0].text, 'valid after malformed progress');
  tool.complete(); await op.promise;
});

test('serialization rechecks lifetime after an extension toJSON cancels the invocation', async () => {
  const f = await fixture(), tool = await heldTool(f), op = tool.start();
  tool.update({ content: [], details: { toJSON() { f.cancel(op.token); return { late: true }; } } });
  assert.equal(tool.signal.aborted, true);
  assert.equal(op.updates.length, 0);
  tool.complete(); await op.promise;
});

test('a rejected native sink remains advisory to the tool result', async () => {
  const f = await fixture(), tool = await heldTool(f);
  const op = tool.start({ sink() { throw new Error('fixture sink is closed'); } });
  assert.doesNotThrow(() => tool.update(content('advisory progress')));
  tool.complete();
  assert.equal((await op.promise).content[0].text, 'done');
});

test('captured collection intrinsics preserve cancellation after public Map tampering', async () => {
  const f = await fixture(), tool = await heldTool(f), op = tool.start();
  vm.runInContext("for (const name of ['get', 'set', 'delete', 'clear', 'forEach']) Map.prototype[name] = () => { throw new Error('tampered Map'); };", f.context);
  tool.update(content('captured lookup works'));
  assert.equal(op.updates.length, 1);
  assert.equal(f.cancel(op.token), true);
  assert.equal(tool.signal.aborted, true);
  tool.update(content('cancelled callback'));
  tool.complete(); await op.promise;
  assert.equal(op.updates.length, 1);
  assert.equal(f.api.count(), 0);
});

test('the private invocation registry reset aborts and drops every retained sink', async () => {
  // Instrument only the actual private registry reset. Full host scrub includes
  // other subsystems and must occur after native drain/quarantine, outside this probe.
  const f = await fixture(), left = await heldTool(f, 'left'), right = await heldTool(f, 'right');
  const first = left.start(), second = right.start();
  left.update(content('left before reset')); right.update(content('right before reset'));
  f.api.resetInvocations();
  assert.equal(left.signal.aborted, true); assert.equal(right.signal.aborted, true);
  assert.equal(f.api.count(), 0);
  left.update(content('left after reset')); right.update(content('right after reset'));
  assert.equal(first.updates.length, 1); assert.equal(second.updates.length, 1);
  left.complete(); right.complete();
  await Promise.all([first.promise, second.promise]);
});

test('an injected owner mismatch suppresses progress without redirecting its sink', async () => {
  // Deliberate source-level fault injection. This does not establish native
  // ownership enforcement or demonstrate a reachable cross-owner runtime state.
  const f = await fixture({ fixedOwner: false }), tool = await heldTool(f), op = tool.start();
  f.api.injectCurrentOwner('different-owner');
  tool.update(content('wrong owner'));
  assert.equal(op.updates.length, 0);
  f.api.injectCurrentOwner(f.owner);
  tool.update(content('original owner'));
  assert.equal(op.updates.length, 1);
  tool.complete(); await op.promise;
});

test('held progress and abort callbacks cannot outlive a completed invocation', async () => {
  const f = await fixture(), tool = await heldTool(f), op = tool.start();
  let aborted = 0;
  tool.signal.addEventListener('abort', () => { aborted++; tool.update(content('abort observer')); });
  tool.complete(); await op.promise;
  await turn();
  assert.equal(f.cancel(op.token), false);
  tool.update(content('late microtask'));
  assert.equal(op.updates.length, 0);
  assert.equal(aborted, 0);
});
