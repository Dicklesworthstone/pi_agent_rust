/**
 * Supplementary published-package rendering and startup diagnostic for issue #279.
 *
 * Requires Node >= 24 and --experimental-vm-modules. Fetch and unpack the
 * unmodified npm package into a NEW directory outside this checkout:
 *
 *   npm_config_user_agent='pi_agent_rust-agent (+https://github.com/Dicklesworthstone/pi_agent_rust)' npm pack --ignore-scripts context-mode@1.0.169
 *   tar -xzf context-mode-1.0.169.tgz
 *   node --experimental-vm-modules scripts/test_published_extension_rendering.mjs --package /path/to/package
 *
 * Use --source /path/to/baseline/extensions_js.rs to run the same diagnostic
 * against another revision's verbatim embedded Intl facade. Use
 * --startup-source /path/to/baseline/extensions.rs to compare startup defaults.
 * The readiness case takes about six seconds with real Node timers.
 *
 * Tarball SRI: sha512-94JIaFuLjF9SO2BsGTrbGtyT44K95+9OC8BdbaL/UT76xOkanJLfUR5CzmNw+GELXZQqH4nBrKg9wjBnSFkVnQ==
 * All consumed package files are separately pinned by SHA-256. The renderer
 * is extracted as one unchanged, contiguous source block; it is not rewritten
 * or vendored. The startup case evaluates the complete, unchanged extension.js
 * and mcp-bridge.js modules with controlled imported dependencies and child I/O.
 *
 * This evaluates the actual published renderer with PiJS's verbatim Intl
 * JavaScript. Node's native Intl supplies a controlled boundary callback AND
 * a reference renderer. Consequently this tests the JS facade integration,
 * iterator semantics and preservation of the published renderer's behavior;
 * it does NOT validate the Rust segmentation engine, QuickJS, full Unicode
 * or Intl conformance, native Windows, PiJS package loading or terminal rendering.
 * The startup case uses real Node timers and a controlled six-second MCP
 * initialize response. The default timeout read from Rust source is applied by
 * a fixture observer: native event admission, configuration, deadline/drop
 * enforcement, quarantine, SQLite, real processes and capabilities are NOT
 * executed. It shows the published hook's readiness requirements only.
 * V8 vm is not a security boundary for arbitrary untrusted packages. DSR
 * remains the authoritative gate: dsr quality --tool pi_agent_rust.
 */

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import EventEmitter from 'node:events';
import { readFileSync, realpathSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';
import vm from 'node:vm';

const { values } = parseArgs({ options: {
  package: { type: 'string' }, source: { type: 'string' },
  'startup-source': { type: 'string' },
  help: { type: 'boolean' },
} });
if (values.help) {
  console.log('Usage: node --experimental-vm-modules scripts/test_published_extension_rendering.mjs --package /path/to/unmodified/package [--source /path/to/extensions_js.rs] [--startup-source /path/to/extensions.rs]');
  process.exit(0);
}
assert(values.package, '--package is required; see the fixture instructions in this script.');
assert(Number.parseInt(process.versions.node, 10) >= 24, 'Requires Node >= 24.');
assert.equal(typeof vm.SourceTextModule, 'function', 'Run with --experimental-vm-modules.');
const packageRoot = realpathSync(values.package);
const sourcePath = values.source ?? fileURLToPath(new URL('../src/extensions_js.rs', import.meta.url));
const rustSource = readFileSync(sourcePath, 'utf8');
const startupSourcePath = values['startup-source'] ?? fileURLToPath(new URL('../src/extensions.rs', import.meta.url));
const startupRustSource = readFileSync(startupSourcePath, 'utf8');
const hashes = {
  'package.json': 'f0a96c5affd66f5334f2f4cd5a1270e65018a0b83317b4e752cd6f60cdba8680',
  'build/adapters/pi/extension.js': '1e6e88988e927bb0c744254b71cd497eaf919e910bea1f0a23f43a3b25ee32c7',
  'build/adapters/pi/mcp-bridge.js': '4a6e89f0bca2674f75076ac009190957ca67cbd4f823926a219adf2c876afd06',
};
const sha256 = text => createHash('sha256').update(text).digest('hex');
const packageSources = new Map();
for (const [file, digest] of Object.entries(hashes)) {
  const bytes = readFileSync(path.join(packageRoot, file));
  assert.equal(sha256(bytes), digest, `Published fixture differs: ${file}`);
  packageSources.set(file, bytes.toString('utf8'));
}
const manifest = JSON.parse(packageSources.get('package.json'));
assert.equal(manifest.name, 'context-mode');
assert.equal(manifest.version, '1.0.169');

function uniqueBlock(source, startMarker, endMarker, label) {
  const start = source.indexOf(startMarker);
  assert(start >= 0, `${label}: start marker missing`);
  assert.equal(source.indexOf(startMarker, start + startMarker.length), -1, `${label}: ambiguous start marker`);
  const end = source.indexOf(endMarker, start);
  assert(end > start, `${label}: end marker missing`);
  return source.slice(start, end);
}
const intl = uniqueBlock(rustSource,
  "if (typeof globalThis.Intl === 'undefined') {", '// Pending hostcalls:', 'embedded Intl');
const renderer = uniqueBlock(packageSources.get('build/adapters/pi/mcp-bridge.js'),
  'export class PiTextComponent', 'function createContextModeCallRenderer', 'published renderer');
function sourceTimeout(name, required = true) {
  const matches = [...startupRustSource.matchAll(new RegExp(`pub const ${name}: u64 = ([\\d_]+);`, 'g'))];
  if (!required && matches.length === 0) return undefined;
  assert.equal(matches.length, 1, `Unique startup timeout constant: ${name}`);
  return Number(matches[0][1].replaceAll('_', ''));
}
const ordinaryEventTimeoutMs = sourceTimeout('EXTENSION_EVENT_TIMEOUT_MS');
const startupTimeoutMs = sourceTimeout('EXTENSION_STARTUP_EVENT_TIMEOUT_MS', false) ?? ordinaryEventTimeoutMs;
const publishedHandshakeTimeout = /const DEFAULT_REQUEST_TIMEOUT_MS = ([\d_]+);/.exec(packageSources.get('build/adapters/pi/mcp-bridge.js'));
assert(publishedHandshakeTimeout, 'Published MCP handshake timeout constant');
const publishedHandshakeTimeoutMs = Number(publishedHandshakeTimeout[1].replaceAll('_', ''));
const initializeDelayMs = 6_000;
console.log('# ' + JSON.stringify({
  package: `${manifest.name}@${manifest.version}`, runtime: process.version,
  platform: process.platform, icuVersion: process.versions.icu,
  unicodeVersion: process.versions.unicode, source: sourcePath,
  packageSha256: hashes, embeddedIntlSha256: sha256(intl), rendererSha256: sha256(renderer),
  startupSource: startupSourcePath, startupSourceSha256: sha256(startupRustSource),
  startupTimeoutMs, ordinaryEventTimeoutMs, publishedHandshakeTimeoutMs, initializeDelayMs,
  verification: 'V8 source diagnostics with controlled Intl, startup-budget observer and child I/O; not Rust, QuickJS, native deadline/cancellation or Windows verification',
}));

const boundaryOracle = new Intl.Segmenter(undefined, { granularity: 'grapheme' });
function fixtureBoundaries(input) {
  assert.equal(typeof input, 'string', 'native boundary callback expects a string');
  assert(input.isWellFormed(), 'the facade must normalize lone surrogates before the Rust string boundary');
  const boundaries = [0];
  for (const record of boundaryOracle.segment(input)) {
    boundaries.push(record.index + record.segment.length);
  }
  return boundaries;
}

async function fixture(useEmbedded) {
  const context = vm.createContext(useEmbedded ? {
    Intl: undefined, __pi_intl_grapheme_boundaries: fixtureBoundaries,
  } : {});
  if (useEmbedded) vm.runInContext(intl, context, { timeout: 2000, filename: sourcePath });
  const module = new vm.SourceTextModule(renderer, {
    context, identifier: `${manifest.name}@${manifest.version}/mcp-bridge.js#renderer`,
  });
  await module.link(name => { throw new Error(`Unexpected renderer import: ${name}`); });
  await module.evaluate({ timeout: 2000 });
  const Segmenter = vm.runInContext('Intl.Segmenter', context);
  return { ...module.namespace, Segmenter, segmenter: new Segmenter(undefined, { granularity: 'grapheme' }) };
}
const [reference, actual] = await Promise.all([fixture(false), fixture(true)]);
const record = item => ({ segment: item.segment, index: item.index, input: item.input });
const records = segments => Array.from(segments, record);

const truncationCases = [
  ['ASCII', 'hello world', 5, 'hello'],
  ['ANSI SGR', '\x1b[31mhello\x1b[0m', 3, '\x1b[31mhel'],
  ['ANSI reset at exact width', '\x1b[31mhello\x1b[0m', 5, '\x1b[31mhello\x1b[0m'],
  ['ZWJ family', 'A👨‍👩‍👧‍👦B', 3, 'A👨‍👩‍👧‍👦'],
  ['regional-indicator flag', '🇺🇸X', 2, '🇺🇸'],
  ['emoji modifier', '👍🏽X', 2, '👍🏽'],
  ['variation selector', '❤\uFE0FX', 2, '❤\uFE0F'],
  ['combining mark', 'e\u0301X', 1, 'e\u0301'],
  ['Indic spacing mark', '\u0915\u093fX', 1, '\u0915\u093f'],
  ['Indic conjunct', '\u0915\u094d\u0937X', 1, '\u0915\u094d\u0937'],
  ['Hangul Jamo', '\u1100\u1161X', 2, '\u1100\u1161'],
  ['before a wide grapheme', '😀X', 1, ''],
  ['exact wide grapheme', '😀X', 2, '😀'],
  ['after a wide grapheme', '😀X', 3, '😀X'],
];
for (const [name, input, width, expected] of truncationCases) {
  test(`published renderer preserves ${name}`, () => {
    assert.equal(reference.truncateAnsiLine(input, width), expected, 'native reference must match the fixed example');
    assert.equal(actual.truncateAnsiLine(input, width), expected);
    const component = new actual.PiTextComponent(input);
    for (let repeat = 0; repeat < 2; repeat++) {
      assert.deepEqual(Array.from(component.render(width)), [expected]);
    }
    component.invalidate();
    assert.deepEqual(Array.from(component.render(width)), [expected]);
  });
}

test('published renderer keeps CRLF, multiline, tab expansion and setText behavior', () => {
  assert.equal(reference.truncateAnsiLine('A\r\nB', 2), 'A\r\nB');
  assert.equal(actual.truncateAnsiLine('A\r\nB', 2), 'A\r\nB');
  const input = 'hello\r\nA👨‍👩‍👧‍👦B\n\tbye';
  const expected = ['hel', 'A👨‍👩‍👧‍👦', '   '];
  assert.deepEqual(Array.from(new reference.PiTextComponent(input).render(3)), expected);
  const component = new actual.PiTextComponent(input);
  assert.deepEqual(Array.from(component.render(3)), expected);
  component.setText('🇺🇸X\r\n👍🏽X');
  assert.deepEqual(Array.from(component.render(2)), ['🇺🇸', '👍🏽']);
});

test('published renderer preserves empty text and nonpositive width behavior', () => {
  for (const implementation of [reference, actual]) {
    for (const input of ['', ' \r\n\t']) {
      assert.deepEqual(Array.from(new implementation.PiTextComponent(input).render(3)), []);
    }
    assert.equal(implementation.truncateAnsiLine('', 3), '');
    for (const width of [0, -1]) {
      assert.equal(implementation.truncateAnsiLine('abc', width), '');
      assert.deepEqual(Array.from(new implementation.PiTextComponent('abc').render(width)), ['a']);
    }
  }
});

test('grapheme records use UTF16 offsets and retain the original input', () => {
  const input = 'A😀e\u0301🇺🇸Z';
  const expected = [
    { segment: 'A', index: 0, input }, { segment: '😀', index: 1, input },
    { segment: 'e\u0301', index: 3, input }, { segment: '🇺🇸', index: 5, input },
    { segment: 'Z', index: 9, input },
  ];
  assert.deepEqual(records(reference.segmenter.segment(input)), expected);
  assert.deepEqual(records(actual.segmenter.segment(input)), expected);
});

test('Segments are reusable and independent iterators expose fresh records', () => {
  const segments = actual.segmenter.segment('A😀B');
  const expected = records(reference.segmenter.segment('A😀B'));
  const first = segments[Symbol.iterator]();
  const second = segments[Symbol.iterator]();
  assert.equal(first[Symbol.iterator](), first);
  const firstRecord = first.next().value;
  assert.deepEqual(record(firstRecord), expected[0]);
  firstRecord.segment = 'mutated';
  firstRecord.index = 99;
  firstRecord.input = 'mutated';
  assert.deepEqual(record(second.next().value), expected[0]);
  assert.deepEqual(record(first.next().value), expected[1]);
  assert.deepEqual(record(second.next().value), expected[1]);
  assert.deepEqual(record(first.next().value), expected[2]);
  assert.equal(first.next().done, true);
  assert.equal(first.next().done, true);
  assert.deepEqual(record(second.next().value), expected[2]);
  assert.equal(second.next().done, true);
  assert.deepEqual(records(segments), expected);
  assert.deepEqual(records(segments), expected);
});

test('empty Segments terminate and can be iterated repeatedly', () => {
  const segments = actual.segmenter.segment('');
  assert.deepEqual(records(segments), []);
  assert.deepEqual(records(segments), []);
  assert.equal(segments.containing(0), undefined);
});

test('containing locates full graphemes using UTF16 positions and returns fresh records', () => {
  const input = 'A😀e\u0301🇺🇸Z';
  const expected = reference.segmenter.segment(input);
  const segments = actual.segmenter.segment(input);
  for (const index of [undefined, NaN, -0.5, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, '3']) {
    assert.deepEqual(record(segments.containing(index)), record(expected.containing(index)));
  }
  for (const index of [-1, -Infinity, input.length, Infinity]) {
    assert.equal(segments.containing(index), undefined);
  }
  segments.containing(1).segment = 'mutated';
  assert.deepEqual(record(segments.containing(2)), record(expected.containing(2)));
  assert.throws(() => segments.containing(Symbol('index')), { name: 'TypeError' });
});

test('lone surrogates are preserved in JS records across the well-formed native boundary', () => {
  for (const input of ['\ud800A\udc00', '\ud800\u0301X', 'x\ud800\ud800\udc00y', '\udc00\u200d😀']) {
    const expected = reference.segmenter.segment(input);
    const segments = actual.segmenter.segment(input);
    const parts = records(segments);
    assert.deepEqual(parts, records(expected));
    assert.equal(parts.map(part => part.segment).join(''), input);
    for (let index = 0; index < input.length; index++) {
      assert.deepEqual(record(segments.containing(index)), record(expected.containing(index)));
    }
  }
});

test('Segmenter uses string coercion and rejects Symbol input', () => {
  for (const input of [undefined, null, 17, { toString: () => '😀A' }]) {
    assert.deepEqual(records(actual.segmenter.segment(input)), records(reference.segmenter.segment(input)));
  }
  assert.throws(() => actual.segmenter.segment(Symbol('input')), { name: 'TypeError' });
});

test('the facade accepts default graphemes and explicitly rejects unsupported granularities', () => {
  assert.deepEqual(records(new actual.Segmenter().segment('👍🏽')), records(reference.segmenter.segment('👍🏽')));
  for (const granularity of ['word', 'sentence', 'invalid']) {
    assert.throws(() => new actual.Segmenter(undefined, { granularity }), { name: 'RangeError' });
  }
  assert.throws(() => new actual.Segmenter(undefined, null), { name: 'TypeError' });
});

async function startupFixture() {
  const timers = new Set(), calls = [], tools = [], diagnostics = [];
  const hooks = new Map(), commands = new Map(), children = [];
  const schedule = (callback, delay, ...args) => {
    const timer = setTimeout(() => { timers.delete(timer); callback(...args); }, delay);
    timers.add(timer);
    return timer;
  };
  const unschedule = timer => { timers.delete(timer); clearTimeout(timer); };
  function spawn(command, args, options) {
    calls.push({ kind: 'spawn', command, args, bridgeDepth: options.env.CONTEXT_MODE_BRIDGE_DEPTH });
    const child = new EventEmitter();
    child.stdout = new EventEmitter();
    child.stderr = new EventEmitter();
    child.exitCode = null;
    child.signalCode = null;
    child.stdin = {
      destroyed: false, writableEnded: false, closed: false,
      write(frame, done) {
        const request = JSON.parse(frame);
        calls.push({ kind: 'rpc', method: request.method });
        done?.(null);
        if (request.id === undefined) return true;
        assert(['initialize', 'tools/list'].includes(request.method), `Unexpected MCP request: ${request.method}`);
        const result = request.method === 'tools/list' ? { tools: [{
          name: 'ctx_readiness_probe', description: 'Controlled transport fixture',
          inputSchema: { type: 'object', properties: {} },
        }] } : {
          protocolVersion: '2025-06-18', capabilities: {},
          serverInfo: { name: 'controlled-transport-fixture', version: '1' },
        };
        schedule(() => {
          if (child.exitCode === null) child.stdout.emit('data', Buffer.from(JSON.stringify({
            jsonrpc: '2.0', id: request.id, result,
          }) + '\n'));
        }, request.method === 'initialize' ? initializeDelayMs : 0);
        return true;
      },
    };
    child.kill = signal => {
      calls.push({ kind: 'kill', signal });
      child.signalCode = signal;
      child.exitCode = 0;
      child.emit('exit', 0, signal);
      return true;
    };
    children.push(child);
    return child;
  }
  const context = vm.createContext({
    process: {
      env: { HOME: '/fixture/home', PWD: '/fixture/workspace', PI_CONFIG_DIR: '/fixture/home/.pi' },
      cwd: () => '/fixture/workspace', execPath: '/fixture/node', platform: 'linux',
    },
    console, Buffer, Intl, setTimeout: schedule, clearTimeout: unschedule,
  });
  const modules = new Map();
  function synthetic(name, exports) {
    modules.set(name, new vm.SyntheticModule(Object.keys(exports), function () {
      for (const [key, value] of Object.entries(exports)) this.setExport(key, value);
    }, { context, identifier: name }));
  }
  synthetic('node:path', { join: path.posix.join, resolve: path.posix.resolve, dirname: path.posix.dirname });
  synthetic('node:url', { fileURLToPath, pathToFileURL });
  synthetic('node:crypto', { createHash });
  synthetic('node:os', { homedir: () => '/fixture/home' });
  synthetic('node:fs', { existsSync: () => true, mkdirSync() {} });
  synthetic('node:child_process', { spawn, execSync: () => '/fixture/node\n' });
  synthetic('../../runtime.js', { detectRuntimes: () => ({ javascript: '/fixture/node' }) });
  synthetic('../detect.js', { foreignWorkspaceEnv: () => [], foreignIdentificationEnv: () => [] });
  synthetic('../../session/db.js', {
    SessionDB: class {
      ensureSession() {}
      cleanupOldSessions() {}
      insertEvent() {}
      getEvents() { return []; }
      getResume() { return null; }
      close() {}
    },
    resolveSessionDbPath: () => '/fixture/home/.pi/context-mode/sessions/probe.db',
  });
  synthetic('../../session/extract.js', {
    extractEvents: () => [], extractUserEvents: () => [],
    parsePiUsage: () => null, buildAgentUsageEvent: () => null,
  });
  synthetic('../../session/snapshot.js', { buildResumeSnapshot: () => '' });
  synthetic('./index.js', { PiAdapter: class { getSessionDir() { return '/fixture/home/.pi/context-mode/sessions'; } } });
  const bridge = new vm.SourceTextModule(packageSources.get('build/adapters/pi/mcp-bridge.js'), {
    context, identifier: pathToFileURL(path.join(packageRoot, 'build/adapters/pi/mcp-bridge.js')).href,
  });
  modules.set('./mcp-bridge.js', bridge);
  const extension = new vm.SourceTextModule(packageSources.get('build/adapters/pi/extension.js'), {
    context, identifier: pathToFileURL(path.join(packageRoot, 'build/adapters/pi/extension.js')).href,
    initializeImportMeta(meta, module) { meta.url = module.identifier; },
  });
  await extension.link(name => {
    assert(modules.has(name), `Unexpected published startup import: ${name}`);
    return modules.get(name);
  });
  await extension.evaluate({ timeout: 2000 });
  extension.namespace.default({
    on(name, handler) { assert(!hooks.has(name)); hooks.set(name, handler); },
    registerTool(tool) { assert(!tools.some(existing => existing.name === tool.name)); tools.push(tool); },
    registerCommand(name, command) { commands.set(name, command); },
    logger: { warn: line => diagnostics.push(line), debug: line => diagnostics.push(line) },
  });
  return {
    calls, tools, diagnostics, hooks, commands, children, schedule, unschedule,
    dispose() { for (const timer of timers) clearTimeout(timer); timers.clear(); },
  };
}

test('published lazy MCP readiness exceeds the old event budget and fits the selected startup default', { timeout: 15_000 }, async () => {
  assert.equal(ordinaryEventTimeoutMs, 5_000, 'the original short event default stays unchanged');
  assert(initializeDelayMs < publishedHandshakeTimeoutMs, 'fixture delay is within the package handshake limit');
  const f = await startupFixture();
  try {
    assert.equal(f.calls.length, 0, 'extension discovery must not start the published MCP bridge');
    f.hooks.get('session_start')({}, { sessionManager: { getSessionFile: () => '/fixture/session.jsonl' } });
    assert.equal(f.calls.length, 0, 'session_start must not start the published MCP bridge');
    let settled = false;
    const startedAt = performance.now();
    const readiness = Promise.resolve(f.hooks.get('before_agent_start')(
      { type: 'before_agent_start', prompt: 'Inspect readiness', systemPrompt: '' }, { hasUI: false },
    )).then(() => { settled = true; return { kind: 'ready' }; });
    // Observers only: unlike the native actor, these timers do not abort or
    // quarantine anything. Keep the same unchanged handler running so its
    // eventual successful handshake distinguishes the budget gap from failure.
    let candidateTimer;
    const candidate = Promise.race([
      readiness,
      new Promise(resolve => { candidateTimer = f.schedule(() => resolve({ kind: 'deadline' }), startupTimeoutMs); }),
    ]).finally(() => f.unschedule(candidateTimer));
    const oldBoundary = await new Promise(resolve => f.schedule(() => resolve({
      settled, tools: f.tools.map(tool => tool.name),
    }), ordinaryEventTimeoutMs));
    assert.equal(oldBoundary.settled, false, 'old deadline arrives during a healthy package handshake');
    assert.deepEqual(oldBoundary.tools, []);
    const completed = await Promise.race([
      readiness,
      new Promise(resolve => f.schedule(() => resolve({ kind: 'fixture-watchdog' }), 5_000)),
    ]);
    assert.equal(completed.kind, 'ready', 'the actual package must finish, not silently degrade or hang');
    const candidateResult = await candidate;
    const elapsedMs = Math.round(performance.now() - startedAt);
    assert.deepEqual(f.tools.map(tool => tool.name), ['ctx_readiness_probe']);
    assert.equal(typeof f.tools[0].execute, 'function');
    assert.deepEqual(f.calls.filter(call => call.kind === 'rpc').map(call => call.method),
      ['initialize', 'notifications/initialized', 'tools/list']);
    assert.equal(f.calls.filter(call => call.kind === 'spawn').length, 1);
    assert.equal(f.calls.find(call => call.kind === 'spawn').bridgeDepth, '1');
    assert.deepEqual(f.diagnostics, [], 'best-effort startup errors must not count as readiness');
    await f.hooks.get('session_shutdown')();
    assert.deepEqual(f.calls.filter(call => call.kind === 'kill').map(call => call.signal), ['SIGTERM']);
    console.log('# ' + JSON.stringify({
      readinessWitness: { oldBoundary, selectedBudgetMs: startupTimeoutMs,
        selectedBudgetOutcome: candidateResult.kind, elapsedMs, tools: f.tools.map(tool => tool.name) },
      verification: 'unchanged published modules, real Node timers, fixture MCP I/O; not native deadline/drop/quarantine or child termination',
    }));
    assert.equal(candidateResult.kind, 'ready', 'the selected startup default must cover this successful published handshake');
  } finally {
    // Fixture cleanup does not stand in for native owner-resource retirement.
    for (const child of f.children) if (child.exitCode === null) child.kill('SIGTERM');
    f.dispose();
  }
});
