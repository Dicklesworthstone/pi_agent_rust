'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const engine = fs.readFileSync(path.join(__dirname,
    '../src/eval/js_kernel/modules/commonjs.js'), 'utf8');
const plain = value => JSON.parse(JSON.stringify(value));

// Execute the exact production cache/wrapper with a controlled admission
// protocol, NOT the native Rust resolver or a sandbox simulation.
function fixture(records) {
    const files = new Map(Object.entries(records));
    const calls = [];
    let active = true;
    const realm = vm.createContext({});
    const host = (operation, identity, name) => {
        calls.push([operation, identity, name]);
        if (!active) throw new Error('EVAL_MODULE_CLOSED: no active cell');
        if (!files.has(identity)) throw new Error('EVAL_MODULE_PATH: unknown identity');
        const slash = identity.indexOf('/');
        const generation = identity.slice(0, slash);
        const filename = identity.slice(slash);
        if (operation === 'check') return '';
        if (operation === 'path') return filename;
        if (operation === 'resolve') {
            if (!name.startsWith('./') && !name.startsWith('../')) {
                throw new Error('EVAL_COMMONJS_SPECIFIER');
            }
            const target = generation + path.posix.resolve(path.posix.dirname(filename), name);
            if (!files.has(target)) throw new Error('EVAL_MODULE_PATH: missing file');
            return target;
        }
        if (operation === 'read') {
            const record = files.get(identity);
            return JSON.stringify({path: filename, directory: path.posix.dirname(filename),
                source: typeof record === 'string' ? record : record.source,
                kind: typeof record === 'string' ? 'commonjs' : record.kind});
        }
        throw new Error('unexpected loader operation');
    };
    const load = vm.runInContext(engine, realm)(host, '__pi_eval_commonjs_loader__');
    return {load, realm, files, calls, close: () => { active = false; },
        reopen: () => { active = true; },
        reads: id => calls.filter(call => call[0] === 'read' && call[1] === id).length};
}

const id = 'g0/work/entry.cjs';

test('exports aliases, top-level this/return and wrapper locals follow CommonJS', () => {
    const f = fixture({[id]: `
        var privateValue = 7;
        exports.start = 1;
        exports.sameThis = this === exports;
        module.exports.answer = 42;
        exports = {discarded: true};
        module.exports.filename = __filename;
        module.exports.dirname = __dirname;
        module.exports.moduleId = module.id;
        module.exports.loadedDuringBody = module.loaded;
        module.exports.inspect = () => [privateValue, module.loaded];
        return;
        module.exports.unreachable = true;
    `});
    const result = f.load(id);
    assert.deepEqual(plain(result), {start: 1, sameThis: true, answer: 42,
        filename: '/work/entry.cjs', dirname: '/work', moduleId: '/work/entry.cjs',
        loadedDuringBody: false});
    assert.deepEqual(plain(result.inspect()), [7, true]);
    assert.equal(vm.runInContext('typeof privateValue', f.realm), 'undefined');
    assert.equal(vm.runInContext('typeof require', f.realm), 'undefined');
});

test('replaced module.exports retains functions, falsy values and undefined', () => {
    for (const source of ['false', 'null', '0', 'undefined', "'text'"]) {
        const f = fixture({[id]: `module.exports = ${source};`});
        const expected = vm.runInContext(source, f.realm);
        assert.equal(f.load(id), expected);
        assert.equal(f.load(id), expected);
        assert.equal(f.reads(id), 1);
    }
    const f = fixture({[id]: 'module.exports = n => n + 2;'});
    assert.equal(f.load(id)(40), 42);
    assert.equal(f.load(id), f.load(id));
});

test('relative transitive dependencies cache once and require.resolve does not execute', () => {
    const f = fixture({
        [id]: "module.exports = {path: require.resolve('./dep.cjs'), run: () => require('./dep.cjs')};",
        'g0/work/dep.cjs': "globalThis.runs = (globalThis.runs || 0) + 1; module.exports = {n: 42};",
    });
    const root = f.load(id);
    assert.equal(root.path, '/work/dep.cjs');
    assert.equal(f.reads('g0/work/dep.cjs'), 0);
    assert.equal(root.run(), root.run());
    assert.equal(root.run().n, 42);
    assert.equal(f.realm.runs, 1);
    assert.equal(f.reads('g0/work/dep.cjs'), 1);
    assert.equal(f.reads(id), 1);
});

test('cycles observe partial exports and the same module instance', () => {
    const f = fixture({
        [id]: "exports.ready = false; const b = require('./b.cjs'); exports.peerSaw = b.saw; exports.ready = true; exports.b = b;",
        'g0/work/b.cjs': "const a = require('./entry.cjs'); exports.saw = a.ready; exports.parent = a;",
    });
    const a = f.load(id);
    assert.equal(a.peerSaw, false);
    assert.equal(a.ready, true);
    assert.equal(a.b.parent, a);
    assert.equal(f.reads(id), 1);
    assert.equal(f.reads('g0/work/b.cjs'), 1);
});

test('JSON remains data, preserves own __proto__, and caches mutations', () => {
    const f = fixture({
        [id]: "module.exports = require('./data.json');",
        'g0/work/data.json': {kind: 'json', source: '\ufeff{"__proto__":{"payload":42},"n":1}'},
    });
    const value = f.load(id);
    assert.equal(Object.hasOwn(value, '__proto__'), true);
    assert.equal(value.__proto__.payload, 42);
    assert.equal(value.payload, undefined);
    value.n = 7;
    assert.equal(f.load('g0/work/data.json').n, 7);
    assert.equal(f.reads('g0/work/data.json'), 1);
});

test('invalid JSON diagnostics do not quote source and failures never cache a result', () => {
    const target = 'g0/work/data.json';
    const f = fixture({[target]: {kind: 'json', source: '{"private-secret": broken}'}});
    assert.throws(() => f.load(target), error => {
        assert.match(error.message, /EVAL_MODULE_JSON/);
        assert.equal(error.message.includes('private-secret'), false);
        return true;
    });
    f.files.set(target, {kind: 'json', source: '42'});
    assert.equal(f.load(target), 42);
    assert.equal(f.reads(target), 2);
});

test('failure removes only the failed module; dependencies and side effects are not replayed', () => {
    const f = fixture({
        [id]: "require('./ok.cjs'); globalThis.tries = (globalThis.tries || 0) + 1; throw new Error('failed');",
        'g0/work/ok.cjs': 'globalThis.okRuns = (globalThis.okRuns || 0) + 1; exports.n = 1;',
    });
    assert.throws(() => f.load(id), /failed/);
    assert.equal(f.realm.tries, 1);
    assert.equal(f.realm.okRuns, 1);
    assert.throws(() => f.load(id), /failed/);
    assert.equal(f.realm.tries, 2);
    assert.equal(f.realm.okRuns, 1);
    f.files.set(id, "module.exports = require('./ok.cjs');");
    assert.equal(f.load(id).n, 1);
    assert.equal(f.reads(id), 3);
});

test('syntax failure runs no prefix and an explicit retry can load repaired source', () => {
    const f = fixture({[id]: 'globalThis.prefix = 1; const broken = ;'});
    assert.throws(() => f.load(id), {name: 'SyntaxError'});
    assert.equal(f.realm.prefix, undefined);
    f.files.set(id, '\ufeff#!/usr/bin/env node\nmodule.exports = 42;');
    assert.equal(f.load(id), 42);
});

test('retained requires and cache hits always ask the current admission owner', () => {
    const f = fixture({[id]: "module.exports = () => require('./dep.cjs');",
        'g0/work/dep.cjs': 'module.exports = {n: 42};'});
    const run = f.load(id);
    const value = run();
    f.close();
    assert.throws(() => f.load(id), /CLOSED/);
    assert.throws(() => run(), /CLOSED/);
    f.reopen();
    assert.equal(run(), value);
    assert.equal(f.reads('g0/work/dep.cjs'), 1);
});

test('old module require closures retain their graph generation', () => {
    const f = fixture({
        [id]: "module.exports = () => require('./dep.cjs').n;",
        'g0/work/dep.cjs': 'exports.n = 1;',
        'g1/work/entry.cjs': "module.exports = () => require('./dep.cjs').n;",
        'g1/work/dep.cjs': 'exports.n = 2;',
    });
    const old = f.load(id);
    const fresh = f.load('g1/work/entry.cjs');
    assert.equal(old(), 1);
    assert.equal(fresh(), 2);
});

test('captured intrinsics survive guest replacement without acquiring new authority', () => {
    const f = fixture({[id]: "module.exports = n => require('./dep.cjs').n + n;",
        'g0/work/dep.cjs': 'exports.n = 40;'});
    const result = f.load(id);
    vm.runInContext(`
        Map.prototype.get = Map.prototype.set = Map.prototype.has = Map.prototype.delete = () => {throw 1;};
        JSON.parse = Function = Reflect.apply = Object.defineProperty = () => {throw 2;};
        String.prototype.slice = String.prototype.charCodeAt = String.prototype.indexOf = () => {throw 3;};
    `, f.realm);
    assert.equal(result(2), 42);
    assert.equal(f.load(id), result);
});

test('deep require chain is bounded and unwinds without retaining partial success', () => {
    const records = {};
    for (let index = 0; index < 65; index++) {
        records[`g0/work/m${index}.cjs`] = index === 64 ? 'module.exports = 42;'
            : `module.exports = require('./m${index + 1}.cjs');`;
    }
    const f = fixture(records);
    assert.throws(() => f.load('g0/work/m0.cjs'), /DEPTH/);
    assert.equal(f.reads('g0/work/m64.cjs'), 0);
    f.files.set('g0/work/m0.cjs', 'module.exports = 7;');
    assert.equal(f.load('g0/work/m0.cjs'), 7);
});

test('invalid require arguments do not coerce objects or run user toString', () => {
    const f = fixture({[id]: 'module.exports = require;'});
    const require = f.load(id);
    let coerced = false;
    for (const argument of [null, undefined, 1, false, {}, '',
        {toString() {coerced = true; return './dep.cjs';}}]) {
        assert.throws(() => require(argument), /SPECIFIER/);
    }
    assert.equal(coerced, false);
    assert.equal(f.calls.filter(call => call[0] === 'resolve').length, 0);
});

test('bootstrap does not supply Node globals or let guest replace its loader', () => {
    const f = fixture({[id]: "module.exports = [typeof process, typeof Buffer, typeof fs, typeof host, typeof cache];"});
    assert.deepEqual(plain(f.load(id)), Array(5).fill('undefined'));
    const property = Object.getOwnPropertyDescriptor(f.realm, '__pi_eval_commonjs_loader__');
    assert.equal(property.writable, false);
    assert.equal(property.configurable, false);
    assert.equal(property.enumerable, false);
});
