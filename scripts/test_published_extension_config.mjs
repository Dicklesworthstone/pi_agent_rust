/**
 * Supplementary published-package diagnostic for issue #279.
 *
 * Requires Node >= 24 and --experimental-vm-modules. Fetch and unpack the
 * unmodified npm package into a NEW directory outside this checkout:
 *
 *   npm_config_user_agent='pi_agent_rust-agent (+https://github.com/Dicklesworthstone/pi_agent_rust)' npm pack --ignore-scripts @zhushanwen/pi-context-engineering@0.2.0
 *   tar -xzf zhushanwen-pi-context-engineering-0.2.0.tgz
 *   node --experimental-vm-modules scripts/test_published_extension_config.mjs --package /path/to/package
 *   node --experimental-vm-modules scripts/test_published_extension_config.mjs --package /path/to/package --windows
 *
 * Tarball SRI: sha512-4TUOTiCAGvu709oi68Yh0p/hGiS1Z9V4jo9P2HtRWD4qo9E5h6eGMqXVeS8jP0N97zxWPtFdV8v6qEn3nkPXAQ==
 * Every source file consumed below is separately pinned by SHA-256. Package
 * source is only type-stripped in memory, never rewritten or vendored.
 *
 * This evaluates the actual published config loader, extension factory and
 * compression lifecycle with PiJS's verbatim fs/path/TypeBox/context JavaScript,
 * controlled native callbacks, UI notifications and an extension API fixture.
 * Host ownership, native filesystem safety, native token estimation, the
 * Rust/QuickJS loader, and Windows execution are NOT tested by this probe.
 * Context-usage values below are explicit host-payload fixtures. Published
 * 0.2.0 treats its L2 threshold as a fraction but reads Pi's 0-100 percent
 * directly; the L2 case records this separate package limitation and uses an
 * explicit threshold of 90 when checking correctly scaled usage delivery.
 * --windows projects path/platform metadata in V8 on the current OS only.
 * V8 vm is not a security boundary for arbitrary untrusted packages. DSR
 * remains the authoritative gate: dsr quality --tool pi_agent_rust.
 */

import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { readFileSync, realpathSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import path from 'node:path';
import { Readable, Writable } from 'node:stream';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import vm from 'node:vm';

const { values } = parseArgs({ options: {
  package: { type: 'string' }, source: { type: 'string' },
  windows: { type: 'boolean' }, help: { type: 'boolean' },
} });
if (values.help) {
  console.log('Usage: node --experimental-vm-modules scripts/test_published_extension_config.mjs --package /path/to/unmodified/package [--windows] [--source /path/to/extensions_js.rs]');
  process.exit(0);
}
assert(values.package, '--package is required; see the fixture instructions in this script.');
assert.equal(typeof vm.SourceTextModule, 'function', 'Run with --experimental-vm-modules.');
const packageRoot = realpathSync(values.package);
const sourcePath = values.source ?? fileURLToPath(new URL('../src/extensions_js.rs', import.meta.url));
const rustSource = readFileSync(sourcePath, 'utf8');
const hashes = {
  'index.ts': '206284adf3e4e0734f607e3b3288c8529d10594a16421a679a2ba7841fd8ffb7',
  'package.json': '70500284eddd476380cbb7709881c3eb2bcacf4c2e5848d3a1663cc0fe3ffcd9',
  'src/commands.ts': '9f249e4e644a5ed7fa6fa9c211c1068f961a6f2ba05b073f3b951bd572cb9e7a',
  'src/compressor.ts': 'bee35f3699de7103de5ac631e9980b49631fb6afcfe83fee5d1c129edad28a9f',
  'src/config.ts': '84b3a37a02b7a354486aedf65c48679d0ea644f23f8879eb14f2bc9a35b67d67',
  'src/frozen-fresh.ts': '8e92347032fe02e5a88b829519429c3055f3ba737a9eb49df1a9f8be20169e2b',
  'src/index.ts': 'b02035d52a00ff94f32bc4946245aba2abc650d612810e53c47ee3afb5f33db1',
  'src/recall-store.ts': '195fdb476a7d0d8b333ade80deb2b7719dde80624481af65edee37a8c47fe814',
  'src/types.ts': '157c9ab2be87c3e5938747ffbc1138c697875118dfadbbf9b8d9889797d400df',
};
const sha256 = text => createHash('sha256').update(text).digest('hex');
const packageSources = new Map();
for (const [file, digest] of Object.entries(hashes)) {
  const bytes = readFileSync(path.join(packageRoot, file));
  assert.equal(sha256(bytes), digest, `Published fixture differs: ${file}`);
  packageSources.set(file, bytes.toString('utf8'));
}
const manifest = JSON.parse(packageSources.get('package.json'));
assert.equal(manifest.name, '@zhushanwen/pi-context-engineering');
assert.equal(manifest.version, '0.2.0');

function embeddedModule(name) {
  const marker = `"${name}".to_string(),\n        compressed_js_literal!(`;
  const start = rustSource.indexOf(marker);
  assert(start >= 0, `Embedded module not found: ${name}`);
  const literal = /^\s*r(#+)"/.exec(rustSource.slice(start + marker.length));
  assert(literal, `Raw module literal missing: ${name}`);
  const body = start + marker.length + literal[0].length;
  const end = rustSource.indexOf('"' + literal[1], body);
  assert(end > body, `Raw module terminator missing: ${name}`);
  return rustSource.slice(body, end);
}
const embedded = new Map(['node:fs', 'node:path', '@sinclair/typebox'].map(name => [name, embeddedModule(name)]));
const contextStart = rustSource.indexOf('function __pi_make_extension_ctx(ctx_payload) {');
const contextEnd = rustSource.indexOf('// Capture the JSON reader and native snapshotter', contextStart);
assert(contextStart >= 0 && contextEnd > contextStart, 'Embedded context builder and model projection not found.');
const embeddedContext = rustSource.slice(contextStart, contextEnd);
console.log('# ' + JSON.stringify({
  package: `${manifest.name}@${manifest.version}`, runtime: process.version,
  platform: process.platform, projectedWindowsPaths: values.windows ?? false,
  source: sourcePath, embeddedSha256: Object.fromEntries([...embedded].map(([name, text]) => [name, sha256(text)])),
  embeddedContextSha256: sha256(embeddedContext),
  verification: 'V8 source diagnostic with fixture host callbacks and usage payloads; not native authorization, token estimation or Windows execution',
}));

async function fixture({ raw, denied = false, ioError = null } = {}) {
  const home = values.windows ? 'C:\\Users\\reporter' : '/fixture/home';
  const cwd = values.windows ? 'C:\\workspace' : '/fixture/workspace';
  const configPath = `${home.replaceAll('\\', '/')}/.pi/agent/extensions/context-engineering/config.json`;
  const virtualPath = `${cwd.replaceAll('\\', '/')}/extension-local.txt`;
  const state = { raw, denied, ioError, reads: [], stats: [], writes: [], readChecks: [], notifications: [] };
  const normalize = input => String(input).replaceAll('\\', '/');
  const errno = (code, message) => Object.assign(new Error(message), { code });
  const authorize = input => {
    const candidate = normalize(input);
    state.readChecks.push(candidate);
    if (state.denied || (candidate !== configPath && candidate !== virtualPath)) {
      throw errno('EACCES', `host read denied: fixture policy for ${candidate}`);
    }
  };
  const context = vm.createContext({
    Buffer, TextEncoder, TextDecoder, Uint8Array, ArrayBuffer,
    process: { platform: values.windows ? 'win32' : 'linux', cwd: () => cwd, env: {} },
    __pi_session_identity: { id: null, file: null, dir: null },
    __pi_current_system_prompt: null,
    __pi_make_extension_ui: () => ({
      notify(message, level) { state.notifications.push({ message, level }); },
    }),
    __pi_get_current_extension_id: () => 'context-engineering',
    __pi_host_temp_roots_json: () => '[]',
    __pi_host_check_read_access: authorize,
    __pi_host_check_write_access(input) {
      state.writes.push(String(input));
      if (normalize(input) === virtualPath) return;
      throw errno('EACCES', 'host write denied: fixture read-only configuration');
    },
    __pi_host_is_registered_fs_path: input => normalize(input) === configPath,
    // The host exposes protected configuration paths through this existing
    // data-path classifier; the mutation callback still denies all writes.
    __pi_host_is_persistent_storage_path: input => normalize(input) === configPath,
    __pi_host_read_file_sync(input) {
      authorize(input);
      state.reads.push(normalize(input));
      if (normalize(input) === virtualPath) throw errno('ENOENT', 'fixture local file does not exist');
      if (state.ioError) throw errno(state.ioError, 'fixture failure; the word ENOENT in a message is not a missing-file code');
      if (state.raw === undefined) throw errno('ENOENT', 'fixture configuration does not exist');
      return Buffer.from(state.raw).toString('base64');
    },
    __pi_host_stat_sync(input) {
      authorize(input);
      state.stats.push(normalize(input));
      if (normalize(input) === virtualPath) throw errno('ENOENT', 'fixture local file does not exist');
      if (state.ioError) throw errno(state.ioError, 'fixture metadata failure; ENOENT text is not a missing-file code');
      if (state.raw === undefined) throw errno('ENOENT', 'fixture configuration does not exist');
      return JSON.stringify({ kind: 'file', size: Buffer.byteLength(state.raw) });
    },
  });
  vm.runInContext(embeddedContext, context, { timeout: 2000, filename: `${sourcePath}#extension-context` });
  function makeContext(payload = {}) {
    return context.__pi_make_extension_ctx({ hasUI: true, cwd, ...payload });
  }
  const modules = new Map();
  function synthetic(name, exports) {
    return new vm.SyntheticModule(Object.keys(exports), function () {
      for (const [key, value] of Object.entries(exports)) this.setExport(key, value);
    }, { context, identifier: name });
  }
  function resolve(specifier, importer = '') {
    if (specifier === 'typebox') return '@sinclair/typebox';
    if (!specifier.startsWith('.')) return specifier;
    const relative = path.posix.normalize(path.posix.join(path.posix.dirname(importer), specifier));
    assert(!relative.startsWith('../'), 'fixture module escapes published package');
    return relative.endsWith('.ts') ? relative : `${relative}.ts`;
  }
  function moduleFor(id) {
    if (modules.has(id)) return modules.get(id);
    let module;
    if (id === 'node:stream') module = synthetic(id, { Readable, Writable });
    else if (id === 'node:os') module = synthetic(id, { homedir: () => home });
    else if (id === 'node:crypto') module = synthetic(id, { randomUUID });
    else {
      const text = embedded.get(id) ?? packageSources.get(id);
      assert(text !== undefined, `Unpinned or unexpected module: ${id}`);
      module = new vm.SourceTextModule(id.endsWith('.ts') ? stripTypeScriptTypes(text, { mode: 'strip' }) : text, { context, identifier: id });
    }
    modules.set(id, module);
    return module;
  }
  async function load(id) {
    const module = moduleFor(id);
    if (module.status === 'unlinked') await module.link((name, importer) => moduleFor(resolve(name, importer.identifier)));
    if (module.status !== 'evaluated') await module.evaluate({ timeout: 2000 });
    return module.namespace;
  }
  const config = await load('src/config.ts');
  return { state, configPath, virtualPath, config, load, makeContext };
}

async function publishedExtension(override) {
  const f = await fixture({ raw: JSON.stringify({ 'context-engineering': override }) });
  const events = new Map(), commands = new Map(), tools = new Map();
  (await f.load('index.ts')).default({
    on(name, handler) { assert(!events.has(name)); events.set(name, handler); },
    registerTool(tool) { assert(!tools.has(tool.name)); tools.set(tool.name, tool); },
    registerCommand(name, command) { assert(!commands.has(name)); commands.set(name, command); },
  });
  return { ...f, events, commands, tools };
}

function compressionConfig(level, options = {}) {
  return {
    mc: { enabled: false }, budget: { enabled: false }, l0: { enabled: false },
    l1: { enabled: false }, l2: { enabled: false },
    [level]: { enabled: true, ...options },
  };
}

function toolMessages() {
  const text = 'ORIGINAL_PAYLOAD_279_'.repeat(100);
  return [
    { role: 'user', content: 'Read the file', timestamp: 1 },
    { role: 'assistant', content: [{ type: 'toolCall', id: 'read-279', name: 'read', arguments: { path: 'fixture.txt' } }], timestamp: 2 },
    { role: 'toolResult', toolCallId: 'read-279', toolName: 'read', content: [{ type: 'text', text }], isError: false, timestamp: 3 },
  ];
}

test('published loader reads its expected configuration layout and merges nested defaults', async () => {
  const f = await fixture({ raw: JSON.stringify({ 'context-engineering': { enabled: false, l0: { bashTruncateChars: 73 }, budget: { previewSize: 17 } } }) });
  const result = f.config.loadConfig();
  assert.equal(result.enabled, false);
  assert.equal(result.l0.bashTruncateChars, 73);
  assert.equal(result.l0.expireMinutes, 30);
  assert.equal(result.budget.previewSize, 17);
  assert.deepEqual(f.state.reads, [f.configPath]);
  assert.equal(f.state.writes.length, 0);
});

test('published loader receives ENOENT.code and uses defaults for an absent configuration', async () => {
  const f = await fixture();
  assert.equal(JSON.stringify(f.config.loadConfig()), JSON.stringify(f.config.DEFAULT_CONFIG));
  const fs = await f.load('node:fs');
  assert.throws(() => fs.readFileSync(f.configPath, 'utf8'), error => error.code === 'ENOENT');
  assert.equal(f.state.writes.length, 0, 'missing configuration must not be created by the package');
});

test('authorization rejection cannot become the package missing-config fallback', async () => {
  const f = await fixture({ denied: true });
  assert.throws(() => f.config.loadConfig(), /Failed to read config.*host read denied/);
  assert.equal(f.state.reads.length, 0, 'denial must precede native reads');
});

for (const code of ['EACCES', 'EIO']) {
  test(`${code} after initial authorization is propagated even when its message contains ENOENT`, async () => {
    const f = await fixture({ ioError: code });
    const fs = await f.load('node:fs');
    assert.throws(() => fs.readFileSync(f.configPath, 'utf8'), error => error.code === code);
    assert.throws(() => f.config.loadConfig(), /Failed to read config.*fixture failure/);
  });
  test(`protected configuration metadata preserves ${code} instead of reporting ENOENT`, async () => {
    const f = await fixture({ ioError: code });
    const fs = await f.load('node:fs');
    assert.throws(() => fs.statSync(f.configPath), error => error.code === code);
    assert.throws(() => fs.lstatSync(f.configPath), error => error.code === code);
    assert.deepEqual(f.state.stats, [f.configPath, f.configPath]);
  });
}

test('configuration metadata reports real byte size and typed ENOENT when the file disappears', async () => {
  const f = await fixture({ raw: '{"context-engineering":{"note":"λ🚀"}}' });
  const fs = await f.load('node:fs');
  const stat = fs.statSync(f.configPath);
  assert.equal(stat.isFile(), true);
  assert.equal(stat.isDirectory(), false);
  assert.equal(stat.size, Buffer.byteLength(f.state.raw));
  f.state.raw = undefined;
  assert.throws(() => fs.statSync(f.configPath), error => (
    error.code === 'ENOENT' && error.syscall === 'stat' && error.path === f.configPath
  ));
  assert.equal(f.state.writes.length, 0);
});

test('invalid configuration is reported instead of falling back to defaults', async () => {
  const f = await fixture({ raw: '{invalid' });
  assert.throws(() => f.config.loadConfig(), /Invalid JSON/);
});

test('published loader rereads configuration and honors revoked read permission', async () => {
  const f = await fixture({ raw: '{"context-engineering":{"enabled":false}}' });
  assert.equal(f.config.loadConfig().enabled, false);
  f.state.raw = '{"context-engineering":{"enabled":true}}';
  assert.equal(f.config.loadConfig().enabled, true);
  f.state.denied = true;
  assert.throws(() => f.config.loadConfig(), /host read denied/);
  assert.equal(f.state.reads.length, 2);
});

test('the published factory registers its real tools and rereads config on session_start', async () => {
  const f = await fixture({ raw: '{"context-engineering":{"enabled":false}}' });
  const extension = await f.load('index.ts');
  const events = new Map(), commands = new Map(), registered = [];
  extension.default({
    on(name, handler) { assert(!events.has(name)); events.set(name, handler); },
    registerTool(tool) { registered.push(tool.name); },
    registerCommand(name, command) { commands.set(name, command); },
  });
  assert.deepEqual(registered, ['recall_context']);
  assert.deepEqual([...commands.keys()].sort(), ['context-engineering', 'context-stats']);
  assert.equal(f.state.reads.length, 1);
  f.state.raw = '{invalid';
  assert.throws(() => events.get('session_start')(), /Invalid JSON/);
  f.state.raw = undefined;
  events.get('session_start')();
  assert.equal(f.state.reads.length, 3);
  assert.equal(f.state.writes.length, 0);
});

test('the actual fs facade propagates read-only write denial without creating a virtual override', async () => {
  const f = await fixture({ raw: '{"context-engineering":{"enabled":true}}' });
  const fs = await f.load('node:fs');
  assert.throws(() => fs.writeFileSync(f.configPath, '{}'), /host write denied/);
  assert.equal(f.config.loadConfig().enabled, true);
  assert.equal(f.state.writes.length, 1);
});

test('ordinary extension-local virtual writes, appends and reads still work', async () => {
  const f = await fixture();
  const fs = await f.load('node:fs');
  assert.throws(() => fs.readFileSync(f.virtualPath, 'utf8'), /ENOENT/);
  fs.writeFileSync(f.virtualPath, 'local');
  fs.appendFileSync(f.virtualPath, ' overlay');
  assert.equal(fs.readFileSync(f.virtualPath, 'utf8'), 'local overlay');
  assert.equal(f.state.raw, undefined, 'virtual writes must not change host configuration');
});

test('the actual context API synchronously returns a fresh usage projection in percentage units', async () => {
  const f = await fixture();
  const ctx = f.makeContext({ contextUsage: {
    tokens: 60_000, contextWindow: 200_000, percent: 0.3,
    privateMetadata: 'must not escape the usage projection',
  } });
  assert.equal(typeof ctx.getContextUsage, 'function');
  const usage = ctx.getContextUsage();
  assert.equal(usage?.then, undefined, 'getContextUsage must be synchronous');
  assert.deepEqual(JSON.parse(JSON.stringify(usage)), { tokens: 60_000, contextWindow: 200_000, percent: 30 });
  usage.tokens = 1;
  usage.percent = 0.0005;
  usage.added = 'extension mutation';
  assert.deepEqual(JSON.parse(JSON.stringify(ctx.getContextUsage())), { tokens: 60_000, contextWindow: 200_000, percent: 30 });
  const overWindow = f.makeContext({ contextUsage: { tokens: 220_000, contextWindow: 200_000, percent: 110 } });
  assert(Math.abs(overWindow.getContextUsage().percent - 110) < 1e-10, 'over-capacity usage must not be clamped');
});

test('usage distinguishes unknown counts, zero tokens and absent authoritative snapshots', async () => {
  const f = await fixture();
  const unknown = f.makeContext({ contextUsage: { tokens: null, contextWindow: 200_000, percent: 30 } });
  assert.deepEqual(JSON.parse(JSON.stringify(unknown.getContextUsage())), { tokens: null, contextWindow: 200_000, percent: null });
  const known = f.makeContext({
    sessionState: { sessionId: 'session-one' },
    contextUsage: { tokens: 0, contextWindow: 200_000, percent: 0 },
  });
  assert.deepEqual(JSON.parse(JSON.stringify(known.getContextUsage())), { tokens: 0, contextWindow: 200_000, percent: 0 });
  assert.equal(f.makeContext({ sessionState: { sessionId: 'session-one' } }).getContextUsage(), undefined);
  assert.equal(f.makeContext({ sessionState: { sessionId: 'session-two' } }).getContextUsage(), undefined);
  assert.equal(f.makeContext().getContextUsage(), undefined, 'usage must not be carried over from another event or session');
});

test('malformed host usage does not become fabricated context data', async () => {
  const f = await fixture();
  for (const contextUsage of [
    null, [], {},
    { tokens: -1, contextWindow: 200_000, percent: -0.0005 },
    { tokens: '60000', contextWindow: 200_000, percent: 30 },
    { tokens: 60_000, contextWindow: 0, percent: 30 },
    { tokens: 60_000, contextWindow: Infinity, percent: 30 },
    { tokens: NaN, contextWindow: 200_000, percent: 30 },
    { tokens: 1.5, contextWindow: 200_000 },
    { tokens: Number.MAX_SAFE_INTEGER + 1, contextWindow: 200_000 },
  ]) {
    assert.equal(f.makeContext({ contextUsage }).getContextUsage(), undefined);
  }
});

test('published context compression, frozen output, recall and statistics work with the actual context API', async () => {
  const f = await publishedExtension(compressionConfig('budget', { maxToolResultCharsPerMessage: 80, previewSize: 12 }));
  const ctx = f.makeContext();
  const messages = toolMessages();
  const original = messages[2].content[0].text;
  const result = f.events.get('context')({ type: 'context', messages }, ctx);
  assert(Array.isArray(result.messages), 'the published catch block must not silently discard compression because getContextUsage is missing');
  const replacement = result.messages[2].content[0].text;
  assert.match(replacement, /^\[Persisted output \(ID: ctx-[a-f0-9]{12}\)/);
  assert(replacement.length < original.length);
  assert.equal(messages[2].content[0].text, original, 'compression must leave the source message intact');
  const id = /ctx-[a-f0-9]{12}/.exec(replacement)[0];
  const recalled = await f.tools.get('recall_context').execute('recall-279', { id }, undefined, undefined, ctx);
  assert.equal(recalled.details.found, true);
  assert.equal(recalled.details.level, 'budget-persisted');
  assert(recalled.content[0].text.endsWith(`\n\n${original}`));
  const repeated = f.events.get('context')({ type: 'context', messages }, ctx);
  assert.equal(repeated.messages[2].content[0].text, replacement, 'frozen output must reuse the same recall ID');
  await f.commands.get('context-stats').handler('', ctx);
  assert.match(f.state.notifications.at(-1).message, /Budget: persisted=1/);
  assert.equal(f.state.notifications.at(-1).level, 'info');
});

test('published commands toggle subsequent context compression through the actual context object', async () => {
  const f = await publishedExtension(compressionConfig('budget', { maxToolResultCharsPerMessage: 80, previewSize: 12 }));
  const ctx = f.makeContext();
  const messages = toolMessages();
  await f.commands.get('context-engineering').handler('global off', ctx);
  assert.equal(f.state.notifications.at(-1).message, 'Context engineering disabled.');
  const disabled = f.events.get('context')({ type: 'context', messages }, ctx);
  assert.equal(disabled.messages, messages);
  await f.commands.get('context-engineering').handler('global on', ctx);
  assert.equal(f.state.notifications.at(-1).message, 'Context engineering enabled.');
  const enabled = f.events.get('context')({ type: 'context', messages }, ctx);
  assert.match(enabled.messages[2].content[0].text, /^\[Persisted output/);
});

test('published session_start clears recall state and statistics while preserving configuration rereads', async () => {
  const f = await publishedExtension(compressionConfig('budget', { maxToolResultCharsPerMessage: 80, previewSize: 12 }));
  const ctx = f.makeContext({ sessionState: { sessionId: 'session-one' } });
  const result = f.events.get('context')({ type: 'context', messages: toolMessages() }, ctx);
  assert(Array.isArray(result.messages), 'context compression must complete before testing session reset');
  const id = /ctx-[a-f0-9]{12}/.exec(result.messages[2].content[0].text)[0];
  const next = f.makeContext({ sessionState: { sessionId: 'session-two' } });
  f.events.get('session_start')({ type: 'session_start' }, next);
  const recalled = await f.tools.get('recall_context').execute('after-reset', { id }, undefined, undefined, next);
  assert.equal(recalled.details.found, false);
  await f.commands.get('context-stats').handler('', next);
  assert.match(f.state.notifications.at(-1).message, /Budget: persisted=0/);
  assert.equal(f.state.reads.length, 2);
  const fresh = f.events.get('context')({ type: 'context', messages: toolMessages() }, next);
  assert.match(fresh.messages[2].content[0].text, /^\[Persisted output/);
  await f.commands.get('context-stats').handler('', next);
  assert.match(f.state.notifications.at(-1).message, /Budget: persisted=1/);
});

test('real percentage usage reaches published L2 with explicitly matching configuration', async t => {
  const f = await publishedExtension(compressionConfig('l2', { emergencyThreshold: 90, protectRecentTurns: 0 }));
  const messages = toolMessages();
  const low = f.makeContext({ contextUsage: { tokens: 60_000, contextWindow: 200_000, percent: 30 } });
  const lowResult = f.events.get('context')({ type: 'context', messages }, low);
  assert(Array.isArray(lowResult.messages));
  assert.equal(lowResult.messages[2].content[0].text, messages[2].content[0].text);
  const high = f.makeContext({ contextUsage: { tokens: 190_000, contextWindow: 200_000, percent: 95 } });
  const highResult = f.events.get('context')({ type: 'context', messages }, high);
  assert.match(highResult.messages[2].content[0].text, /^\[Tool result expired/);

  // Record the package's independent units mismatch without adapting Pi's API
  // or presenting this observation as a successful default L2 behavior check.
  const publishedDefault = await publishedExtension(compressionConfig('l2', { protectRecentTurns: 0 }));
  const defaultContext = publishedDefault.makeContext({ contextUsage: { tokens: 60_000, contextWindow: 200_000, percent: 30 } });
  const defaultResult = publishedDefault.events.get('context')({ type: 'context', messages }, defaultContext);
  t.diagnostic(JSON.stringify({
    knownPackageLimitation: '0.2.0 compares a fraction threshold directly with Pi percentage units',
    configuredDefaultThreshold: publishedDefault.config.DEFAULT_CONFIG.l2.emergencyThreshold,
    suppliedPercent: defaultContext.getContextUsage().percent,
    expiredAtThirtyPercent: defaultResult.messages?.[2].content[0].text.startsWith('[Tool result expired'),
    verification: 'observed package limitation; not default-L2 compatibility approval',
  }));
});
