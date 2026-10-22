/**
 * Network-free diagnostic for the actual embedded node:http server source.
 * Run: node --experimental-vm-modules scripts/pijs_virtual_request_probe.mjs
 * Compare another source: add --source /path/to/src/http_shim.rs
 *
 * This V8 diagnostic is not QuickJS/Rust, native Windows, or real Freeflow
 * validation. The listen bridge, microtasks, timers, EventEmitter and decoder
 * are fixtures. DSR remains the authoritative gate:
 *   dsr quality --tool pi_agent_rust
 */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import EventEmitter from 'node:events';
import { StringDecoder } from 'node:string_decoder';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import vm from 'node:vm';
import test from 'node:test';

const { values } = parseArgs({ options: { source: { type: 'string' } } });
const sourcePath = values.source ?? fileURLToPath(new URL('../src/http_shim.rs', import.meta.url));
const rust = readFileSync(sourcePath, 'utf8');
const marker = 'pub const NODE_HTTP_JS: &str = r#"';
const start = rust.indexOf(marker);
assert(start >= 0 && rust.indexOf(marker, start + marker.length) === -1, 'unique HTTP source marker');
const end = rust.indexOf('"#;', start + marker.length);
assert(end > start, 'HTTP source terminator');
const source = rust.slice(start + marker.length, end);
console.log(`# HTTP source sha256=${createHash('sha256').update(source).digest('hex')}`);

// Model the documented PiJS emitter's exception isolation rather than rely on
// Node's stricter default. The HTTP-specific emitter in the source must enforce
// its own failure boundary; a Node-only emitter would mask that defect.
class IsolatingEmitter extends EventEmitter {
  emit(event, ...args) {
    const listeners = this.rawListeners(event).slice();
    for (const listener of listeners) {
      try { Reflect.apply(listener, this, args); } catch (_) {}
    }
    return listeners.length > 0;
  }
}

async function fixture(listener) {
  const tasks = [], timers = new Map(), transactions = new Set();
  let factory, closed, nextTimer = 0;
  const context = vm.createContext({
    Buffer, TextEncoder, TextDecoder, URL, AbortController, AbortSignal,
    queueMicrotask(callback) { tasks.push(callback); },
    setTimeout(callback, ms) { const id = ++nextTimer; timers.set(id, { callback, ms }); return id; },
    clearTimeout(id) { timers.delete(id); },
    __pi_base64_decode_native: value => Buffer.from(value, 'base64').toString('latin1'),
    __pi_base64_encode_bytes_native: value => Buffer.from(value).toString('base64'),
    __pi_virtual_http_listen(host, port, handler, onClose) {
      assert.equal(host, '127.0.0.1');
      factory = handler;
      closed = onClose;
      return { host, port: port || 28180,
        close() { for (const tx of transactions) tx.cancel(); closed(); },
        cancelRequests() { for (const tx of transactions) tx.cancel(); },
      };
    },
    fetch() { throw new Error('Unexpected external network dispatch'); },
  });
  const events = new vm.SyntheticModule(['default'], function () {
    this.setExport('default', IsolatingEmitter);
  }, { context });
  const decoder = new vm.SyntheticModule(['StringDecoder'], function () {
    this.setExport('StringDecoder', StringDecoder);
  }, { context });
  const module = new vm.SourceTextModule(source, { context, identifier: 'actual-node-http-shim' });
  await module.link(name => {
    if (name === 'node:events') return events;
    if (name === 'node:string_decoder') return decoder;
    throw new Error(`Unexpected module: ${name}`);
  });
  await module.evaluate({ timeout: 1000 });
  const server = module.namespace.createServer(listener);
  server.listen(28180, '127.0.0.1');
  const drain = (limit = 2048) => {
    let count = 0;
    while (tasks.length > 0) {
      assert(count++ < limit, 'microtasks must not busy-spin while paused');
      tasks.shift()();
    }
  };
  drain();
  return {
    server, tasks, timers, drain,
    transaction(body = Buffer.alloc(0), path = '/v1/chat/completions?q=1', headers = {}) {
      const tx = factory({ url: `http://127.0.0.1:28180${path}`, method: 'POST', headers,
        body_bytes: Buffer.from(body).toString('base64') });
      transactions.add(tx);
      return tx;
    },
    expire() {
      const pending = [...timers.values()];
      timers.clear();
      for (const { callback } of pending) callback();
      drain();
    },
  };
}

function bodyOf(tx) {
  const chunks = [];
  let status;
  for (let i = 0; i < 2048; i++) {
    const item = tx.poll();
    if (item.done) return { status, body: Buffer.concat(chunks) };
    assert(!item.value.pending, 'response must complete without waiting');
    if (item.value.status !== undefined) status = item.value.status;
    if (item.value.body_bytes !== undefined) chunks.push(Buffer.from(item.value.body_bytes, 'base64'));
  }
  assert.fail('response did not terminate');
}

const payload = () => Buffer.from(Array.from({ length: 40003 }, (_, i) => i % 256));

test('binary uploads retain routing and bytes in bounded independent chunks', async () => {
  let request, ends = 0;
  const chunks = [];
  const f = await fixture((req, res) => {
    request = req;
    req.on('data', bytes => chunks.push(bytes));
    req.on('end', () => { ends++; res.end(Buffer.concat(chunks)); });
  });
  const bytes = payload(), tx = f.transaction(bytes, '/v1/chat/completions?q=1', { 'X-Token': 'fixture' });
  f.drain();
  assert.equal(ends, 1);
  assert.equal(request.complete, true);
  assert.equal(request.url, '/v1/chat/completions?q=1');
  assert.equal(request.headers['x-token'], 'fixture');
  assert.equal(request.headers.host, '127.0.0.1:28180');
  assert.equal(request.method, 'POST');
  assert(chunks.length > 1);
  assert(chunks.every(c => Buffer.isBuffer(c) && c.length <= 16 * 1024));
  assert.deepEqual(bodyOf(tx).body, bytes);
  chunks[0].fill(0);
  assert.deepEqual(chunks[1], bytes.subarray(16384, 32768));
});

test('pause in handler prevents data/end until idempotent resume', async () => {
  let request, ends = 0;
  const chunks = [];
  const f = await fixture((req, res) => {
    request = req;
    assert.equal(req.pause(), req);
    req.on('data', c => chunks.push(c));
    req.on('end', () => { ends++; res.end('ok'); });
  });
  const tx = f.transaction(payload());
  f.drain();
  assert.equal(chunks.length, 0);
  assert.equal(ends, 0);
  assert.equal(request.isPaused(), true);
  assert.equal(request.complete, false);
  assert.equal(request.resume(), request);
  request.resume();
  f.drain();
  assert.equal(ends, 1);
  assert.equal(request.isPaused(), false);
  assert.deepEqual(Buffer.concat(chunks), payload());
  assert.equal(bodyOf(tx).body.toString(), 'ok');
});

test('pause after first chunk prevents read-ahead and duplicate resume deliveries', async () => {
  let request, ends = 0;
  const chunks = [];
  const f = await fixture((req, res) => {
    request = req;
    req.on('data', c => { chunks.push(c); if (chunks.length === 1) req.pause(); });
    req.on('end', () => { ends++; res.end('ok'); });
  });
  const tx = f.transaction(payload());
  f.drain();
  assert.equal(chunks.length, 1);
  assert.equal(chunks[0].length, 16384);
  assert.equal(ends, 0);
  assert.equal(f.tasks.length, 0);
  request.resume(); request.resume();
  f.drain();
  assert.equal(ends, 1);
  assert.deepEqual(Buffer.concat(chunks), payload());
  assert.equal(bodyOf(tx).status, 200);
});

test('pausing the last data frame defers end without redelivering bytes', async () => {
  let request, chunks = 0, ends = 0;
  const f = await fixture((req, res) => {
    request = req;
    req.on('data', () => { chunks++; req.pause(); });
    req.on('end', () => { ends++; res.end(); });
  });
  const tx = f.transaction('last');
  f.drain();
  assert.equal(chunks, 1); assert.equal(ends, 0);
  request.resume(); f.drain();
  assert.equal(chunks, 1); assert.equal(ends, 1);
  assert.equal(bodyOf(tx).status, 200);
});

test('destroy in data prevents end/upstream work and notifies every cleanup listener once', async () => {
  let request, ends = 0, aborted = 0, closes = 0, socketCloses = 0, responseCloses = 0;
  const f = await fixture((req, res) => {
    request = req;
    req.on('aborted', () => { aborted++; });
    req.on('close', () => { throw new Error('throwing observer'); });
    req.once('close', () => { closes++; });
    req.socket.on('close', () => { socketCloses++; });
    res.on('close', () => { responseCloses++; });
    req.on('data', () => req.destroy('fixture rejection'));
    req.on('end', () => { ends++; res.end('must not happen'); });
  });
  const tx = f.transaction(payload());
  f.drain();
  assert.equal(ends, 0);
  assert.equal(request.complete, false); assert.equal(request.aborted, true);
  assert.throws(() => tx.poll(), /fixture rejection/);
  tx.cancel(); request.resume(); f.drain();
  assert.deepEqual([aborted, closes, socketCloses, responseCloses], [1, 1, 1, 1]);
  assert.equal(request.listenerCount('close'), 1, 'once listener removed');
});

test('cancellation queued by final data wins over end', async () => {
  let ends = 0;
  const f = await fixture((req, res) => {
    req.on('data', () => f.tasks.push(() => req.destroy('queued cancellation')));
    req.on('end', () => { ends++; res.end(); });
  });
  const tx = f.transaction('one frame');
  f.drain();
  assert.equal(ends, 0);
  assert.throws(() => tx.poll(), /queued cancellation/);
});

test('early response in data stops request delivery and retains the rejection response', async () => {
  let ends = 0, count = 0, request;
  const f = await fixture((req, res) => {
    request = req;
    req.on('data', () => { count++; res.writeHead(413); res.end('rejected'); });
    req.on('end', () => { ends++; });
  });
  const tx = f.transaction(payload());
  f.drain();
  assert.deepEqual([count, ends, request.complete], [1, 0, false]);
  const response = bodyOf(tx);
  assert.equal(response.status, 413); assert.equal(response.body.toString(), 'rejected');
});

test('a throwing data callback fails the transaction instead of continuing the relay', async () => {
  let ends = 0, closed = 0;
  const f = await fixture((req, res) => {
    req.on('data', () => { throw new Error('data failure'); });
    req.on('end', () => { ends++; res.end('wrong success'); });
    res.on('close', () => { closed++; });
  });
  const tx = f.transaction(payload());
  f.drain();
  assert.throws(() => tx.poll(), /data failure/);
  assert.equal(ends, 0); assert.equal(closed, 1);
});

test('a throwing end callback closes the transaction without a false aborted notification', async () => {
  let request, aborted = 0, closed = 0;
  const f = await fixture((req, res) => {
    request = req;
    req.on('data', () => {});
    req.on('aborted', () => { aborted++; });
    req.on('end', () => { throw new Error('end failure'); });
    res.on('close', () => { closed++; });
  });
  const tx = f.transaction('complete upload');
  f.drain();
  assert.throws(() => tx.poll(), /end failure/);
  assert.deepEqual([request.complete, request.aborted, aborted, closed], [true, false, 0, 1]);
});

test('UTF-8 split across chunk boundaries and the decoder tail remain intact', async () => {
  const bytes = Buffer.concat([Buffer.alloc(16383, 97), Buffer.from('☃🎼é'), Buffer.from([0xe2, 0x82])]);
  const chunks = [];
  const f = await fixture((req, res) => {
    req.setEncoding('utf8');
    req.on('data', c => chunks.push(c));
    req.on('end', () => res.end(chunks.join('')));
  });
  const tx = f.transaction(bytes);
  f.drain();
  assert(chunks.every(c => typeof c === 'string'));
  assert.equal(chunks.join(''), bytes.toString('utf8'));
  assert.equal(bodyOf(tx).body.toString(), bytes.toString('utf8'));
});

test('pause from decoder tail defers end and never duplicates the tail', async () => {
  let request, ends = 0;
  const chunks = [];
  const f = await fixture((req, res) => {
    request = req; req.setEncoding('utf8');
    req.on('data', c => { chunks.push(c); req.pause(); });
    req.on('end', () => { ends++; res.end(); });
  });
  const tx = f.transaction(Buffer.from([0xe2]));
  f.drain();
  assert.deepEqual(chunks, ['�']); assert.equal(ends, 0);
  request.resume(); f.drain();
  assert.deepEqual(chunks, ['�']); assert.equal(ends, 1);
  assert.equal(bodyOf(tx).status, 200);
});

test('cancelling a paused upload settles waiting consumers and cannot be resumed', async () => {
  let request, data = 0, ends = 0;
  const f = await fixture(req => {
    request = req; req.pause();
    req.on('data', () => { data++; }); req.on('end', () => { ends++; });
  });
  const tx = f.transaction(payload());
  f.drain();
  let state = 'pending';
  const waiting = tx.next().then(() => { state = 'wrong success'; }, () => { state = 'rejected'; });
  tx.cancel('stop'); request.resume(); f.drain();
  await waiting;
  assert.deepEqual([state, data, ends, request.complete], ['rejected', 0, 0, false]);
});

test('cancellation queued by decoder tail wins over end', async () => {
  let ends = 0;
  const f = await fixture((req, res) => {
    req.setEncoding('utf8');
    req.on('data', () => f.tasks.push(() => req.destroy('tail cancellation')));
    req.on('end', () => { ends++; res.end(); });
  });
  const tx = f.transaction(Buffer.from([0xe2]));
  f.drain();
  assert.equal(ends, 0);
  assert.throws(() => tx.poll(), /tail cancellation/);
});

test('empty body sends end once and no data', async () => {
  let data = 0, ends = 0;
  const f = await fixture((req, res) => {
    req.on('data', () => { data++; }); req.on('end', () => { ends++; res.end('empty'); });
  });
  const tx = f.transaction();
  f.drain();
  assert.deepEqual([data, ends], [0, 1]);
  assert.equal(bodyOf(tx).body.toString(), 'empty');
});

test('the existing 10 MiB request bound rejects before invoking the handler', async () => {
  let calls = 0;
  const f = await fixture(() => { calls++; });
  assert.throws(() => f.transaction(Buffer.alloc(10 * 1024 * 1024 + 1)), /10 MiB/);
  f.drain(); assert.equal(calls, 0);
});

test('the existing 1 MiB response queue bound is unchanged', async () => {
  const f = await fixture((_req, res) => res.end(Buffer.alloc(1024 * 1024 + 1)));
  const tx = f.transaction();
  f.drain(); assert.throws(() => tx.poll(), /1 MiB/);
});

test('pausing one request never pauses a concurrent transaction', async () => {
  let paused, secondDone = false;
  const f = await fixture((req, res) => {
    if (req.url === '/paused') { paused = req; req.pause(); }
    req.on('data', () => {});
    req.on('end', () => { if (req.url === '/active') secondDone = true; res.end(req.url); });
  });
  const first = f.transaction(payload(), '/paused'), second = f.transaction(payload(), '/active');
  f.drain();
  assert.equal(secondDone, true); assert.equal(paused.complete, false);
  assert.equal(bodyOf(second).body.toString(), '/active');
  paused.resume(); f.drain();
  assert.equal(bodyOf(first).body.toString(), '/paused');
});

test('a paused request still obeys its timeout and cancels once', async () => {
  let data = 0, ended = 0, closed = 0, request;
  const f = await fixture((req, res) => {
    request = req; req.pause(); req.setTimeout(25);
    req.on('data', () => { data++; }); req.on('end', () => { ended++; });
    res.on('close', () => { closed++; });
  });
  const tx = f.transaction(payload());
  f.drain();
  assert.equal(f.timers.size, 1);
  f.expire(); request.resume(); f.drain();
  assert.throws(() => tx.poll(), /timed out/);
  assert.deepEqual([data, ended, closed, f.timers.size], [0, 0, 1, 0]);
});

test('normal completion disarms the request timeout', async () => {
  const f = await fixture((req, res) => {
    req.setTimeout(25); req.on('data', () => {}); req.on('end', () => res.end('complete'));
  });
  const tx = f.transaction(payload()); f.drain();
  assert.equal(f.timers.size, 0);
  f.expire(); assert.equal(bodyOf(tx).body.toString(), 'complete');
});
