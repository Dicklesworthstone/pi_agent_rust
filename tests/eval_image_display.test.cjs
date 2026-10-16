// Run without project dependencies: node --test tests/eval_image_display.test.cjs
// Exercises the actual embedded JS factory. Native QuickJS/host image admission
// remains covered by Rust tests; this harness does not replace that validation.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { createContext, runInContext } = require('node:vm');
const source = readFileSync(join(__dirname, '../src/eval/js_kernel/display.js'), 'utf8');

function kernel() {
    const frames = [];
    const context = createContext({ sink: frame => frames.push(JSON.parse(frame)) });
    runInContext(`globalThis.display = (${source})(sink)`, context);
    return { frames, run: code => runInContext(code, context) };
}

for (const mime of ['image/png', 'image/jpeg', 'image/gif', 'image/webp']) {
    test(`${mime}: byte views and base64 are snapshotted without coercion`, () => {
        const k = kernel();
        k.run(`const bytes = new Uint8Array([7, 0, 127, 128, 255, 6]);
            display(bytes.subarray(1, 5), ${JSON.stringify(mime)}); bytes.fill(42);`);
        assert.deepEqual(k.frames, [{ mimeType: mime, data: Buffer.from([0, 127, 128, 255]).toString('base64') }]);
        k.run(`display('YWJj', ${JSON.stringify(mime)})`);
        assert.deepEqual(k.frames[1], { mimeType: mime, data: 'YWJj' });
    });
}

test('encoder handles every base64 remainder and all byte values', () => {
    const k = kernel();
    for (let length = 0; length <= 260; length++) {
        k.run(`display(Uint8Array.from({length:${length}}, (_, i) => i), 'image/jpeg')`);
        const bytes = Buffer.from(Array.from({ length }, (_, i) => i & 255));
        assert.equal(k.frames[length].data, bytes.toString('base64'));
    }
});

test('all image formats reject arrays, non-byte views and coercion hooks', () => {
    const k = kernel();
    for (const value of ['[1, 2]', 'new Uint16Array([1])', 'new DataView(new ArrayBuffer(8))',
        "({toString(){throw new Error('coerced')}})", 'new Proxy(new Uint8Array(2), {})']) {
        assert.throws(() => k.run(`display(${value}, 'image/webp')`), /EVAL_DISPLAY_INVALID/);
    }
    assert.equal(k.frames.length, 0);
});

test('SVG, HTML and arbitrary MIME types are still refused', () => {
    const k = kernel();
    for (const mime of ['image/svg+xml', 'text/html', 'image/jpg', 'application/octet-stream']) {
        assert.throws(() => k.run(`display('abc', ${JSON.stringify(mime)})`), /EVAL_DISPLAY_INVALID/);
    }
    assert.equal(k.frames.length, 0);
});

test('image admission preserves per-frame limits and publishes no partial frame', () => {
    const k = kernel();
    assert.throws(() => k.run("display(new Uint8Array(1572865), 'image/jpeg')"), /EVAL_DISPLAY_LIMIT/);
    assert.throws(() => k.run("display('a'.repeat(2097152), 'image/gif')"), /EVAL_DISPLAY_LIMIT/);
    assert.equal(k.frames.length, 0);
});

test('existing JSON, markdown and snapshot semantics remain intact', () => {
    const k = kernel();
    k.run("const data = {answer: [42, true, null]}; display(data); data.answer[0] = 0; display('## Answer', 'text/markdown')");
    assert.deepEqual(k.frames, [
        { mimeType: 'application/json', data: { answer: [42, true, null] } },
        { mimeType: 'text/markdown', data: '## Answer' },
    ]);
    assert.throws(() => k.run('display({get value(){throw new Error("getter invoked")}})'), /EVAL_DISPLAY_INVALID/);
    assert.throws(() => k.run('const cyclic = {}; cyclic.self = cyclic; display(cyclic)'), /cyclic JSON/);
    assert.equal(k.frames.length, 2);
});

test('captured intrinsics survive guest replacement', () => {
    const k = kernel();
    k.run(`const bytes = new Uint8Array([255, 216, 255, 217]);
        JSON.stringify = () => {throw new Error('tampered')};
        ArrayBuffer.isView = () => false;
        Reflect.apply = () => {throw new Error('tampered')};
        display(bytes, 'image/jpeg');`);
    assert.deepEqual(k.frames, [{ mimeType: 'image/jpeg', data: '/9j/2Q==' }]);
});
