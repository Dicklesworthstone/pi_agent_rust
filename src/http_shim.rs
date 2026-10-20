//! Node.js `http` and `https` shim — pure-JS implementation for the QuickJS
//! extension runtime.
//!
//! Provides `http.request`, `http.get`, `https.request`, `https.get` that route
//! all HTTP traffic through the capability-gated `pi.http()` hostcall. Uses the
//! `EventEmitter` from `node:events` for the standard Node.js event-based API.

/// The JS source for the `node:http` virtual module.
pub const NODE_HTTP_JS: &str = r#"
import EventEmitter from "node:events";
import { StringDecoder } from "node:string_decoder";

// ─── STATUS_CODES ────────────────────────────────────────────────────────────

const STATUS_CODES = {
  200: 'OK', 201: 'Created', 204: 'No Content',
  301: 'Moved Permanently', 302: 'Found', 304: 'Not Modified',
  400: 'Bad Request', 401: 'Unauthorized', 403: 'Forbidden',
  404: 'Not Found', 405: 'Method Not Allowed', 408: 'Request Timeout',
  500: 'Internal Server Error', 502: 'Bad Gateway', 503: 'Service Unavailable',
};

const METHODS = [
  'GET', 'HEAD', 'POST', 'PUT', 'DELETE', 'CONNECT',
  'OPTIONS', 'TRACE', 'PATCH',
];

function __pi_http_is_binary_chunk(chunk) {
  if (typeof Buffer !== 'undefined' && Buffer.isBuffer && Buffer.isBuffer(chunk)) {
    return true;
  }
  if (chunk instanceof Uint8Array || chunk instanceof ArrayBuffer) {
    return true;
  }
  return !!(ArrayBuffer.isView && ArrayBuffer.isView(chunk));
}

function __pi_http_to_uint8(chunk) {
  if (typeof Buffer !== 'undefined' && Buffer.isBuffer && Buffer.isBuffer(chunk)) {
    return new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  }
  if (chunk instanceof Uint8Array) {
    return chunk;
  }
  if (chunk instanceof ArrayBuffer) {
    return new Uint8Array(chunk);
  }
  if (ArrayBuffer.isView && ArrayBuffer.isView(chunk)) {
    return new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  }
  return new TextEncoder().encode(String(chunk ?? ''));
}

function __pi_http_clone_body_chunk(chunk) {
  const view = __pi_http_to_uint8(chunk);
  if (typeof Buffer !== 'undefined' && typeof Buffer.from === 'function') {
    return Buffer.from(view);
  }
  return new Uint8Array(view);
}

function __pi_http_chunks_to_base64(chunks) {
  const parts = chunks.map((chunk) => __pi_http_to_uint8(chunk));
  const total = parts.reduce((sum, part) => sum + part.byteLength, 0);
  const merged =
    typeof Buffer !== 'undefined' && typeof Buffer.alloc === 'function'
      ? Buffer.alloc(total)
      : new Uint8Array(total);

  let offset = 0;
  for (const part of parts) {
    merged.set(part, offset);
    offset += part.byteLength;
  }

  if (typeof globalThis.__pi_base64_encode_bytes_native === 'function') {
    return __pi_base64_encode_bytes_native(merged);
  }

  // Fallback for older runtime bounds
  let binary = '';
  let chunk = [];
  for (let i = 0; i < merged.length; i++) {
    chunk.push(merged[i]);
    if (chunk.length >= 4096) {
      binary += String.fromCharCode.apply(null, chunk);
      chunk.length = 0;
    }
  }
  if (chunk.length > 0) {
    binary += String.fromCharCode.apply(null, chunk);
  }
  return __pi_base64_encode_native(binary);
}

function __pi_http_decode_body_bytes(bodyBytes) {
  const encoded = String(bodyBytes ?? '');
  const binary = globalThis.__pi_base64_decode_native(encoded);
  const out =
    typeof Buffer !== 'undefined' && typeof Buffer.alloc === 'function'
      ? Buffer.alloc(binary.length)
      : new Uint8Array(binary.length);

  for (let i = 0; i < binary.length; i++) {
    out[i] = binary.charCodeAt(i) & 0xff;
  }
  return out;
}

function __pi_http_decode_chunk(chunk, encoding) {
  if (!encoding || typeof chunk === 'string') {
    return chunk;
  }

  const bytes = __pi_http_to_uint8(chunk);
  if (typeof Buffer !== 'undefined' && typeof Buffer.from === 'function') {
    return Buffer.from(bytes).toString(encoding);
  }
  return new TextDecoder(encoding).decode(bytes);
}

// ─── IncomingMessage ─────────────────────────────────────────────────────────

class IncomingMessage extends EventEmitter {
  constructor(statusCode, headers, body) {
    super();
    this.statusCode = statusCode;
    this.statusMessage = STATUS_CODES[statusCode] || 'Unknown';
    this.headers = headers || {};
    this._body = body || '';
    this._destroyed = false;
    this.destroyed = false;
    this._reader = null;
    this._paused = false;
    this._resumeRead = null;
    this._cancelRequest = null;
    this.complete = false;
    this.httpVersion = '1.1';
    this.method = null;
    this.url = '';
  }

  _deliver() {
    if (this._destroyed) {
      return;
    }

    const chunk = __pi_http_decode_chunk(this._body, this._encoding);
    if (chunk && chunk.length > 0) {
      this.emit('data', chunk);
    }

    if (this._destroyed) {
      return;
    }

    this.complete = true;
    this.emit('end');
  }

  setEncoding(encoding) {
    this._encoding = encoding ? String(encoding) : 'utf8';
    this._decoder = new StringDecoder(this._encoding);
    return this;
  }
  resume() {
    this._paused = false;
    if (this._resumeRead) { const resume = this._resumeRead; this._resumeRead = null; resume(); }
    return this;
  }
  pause() { this._paused = true; return this; }
  destroy(error) {
    if (this._destroyed) {
      return this;
    }
    this._destroyed = true;
    this.destroyed = true;
    try { if (this._reader) Promise.resolve(this._reader.cancel(error)).catch(() => {}); } catch (_) {}
    try { if (this._cancelRequest) this._cancelRequest(error); } catch (_) {}
    this.resume();
    if (error) this.emit('error', error);
    this.emit('close');
    return this;
  }

  async _deliverWeb(body, onActivity) {
    if (!body || this._destroyed) {
      if (!this._destroyed) { this.complete = true; this.emit('end'); this.emit('close'); }
      return;
    }
    const reader = body.getReader();
    this._reader = reader;
    try {
      while (!this._destroyed) {
        if (this._paused) await new Promise(resolve => { this._resumeRead = resolve; });
        if (this._destroyed) break;
        const { done, value } = await reader.read();
        if (done || this._destroyed) break;
        onActivity();
        const bytes = __pi_http_clone_body_chunk(value);
        const chunk = this._decoder ? this._decoder.write(bytes) : bytes;
        if (chunk.length > 0) this.emit('data', chunk);
      }
      if (!this._destroyed) {
        const tail = this._decoder ? this._decoder.end() : '';
        if (tail) this.emit('data', tail);
        if (!this._destroyed) { this.complete = true; this.emit('end'); this.emit('close'); }
      }
    } catch (error) {
      if (!this._destroyed) this.destroy(error);
    } finally {
      this._reader = null;
      try { reader.releaseLock(); } catch (_) {}
    }
  }
}

// ─── ClientRequest ───────────────────────────────────────────────────────────

class ClientRequest extends EventEmitter {
  constructor(options, callback) {
    super();
    this._options = options;
    this._body = [];
    this._ended = false;
    this._sent = false;
    this._aborted = false;
    this.destroyed = false;
    this._controller = new AbortController();
    this._response = null;
    this._timeoutId = null;
    this._timeoutMs = Number(options.timeout) > 0 ? Number(options.timeout) : 0;
    this._headers = {};
    this.socket = { remoteAddress: '127.0.0.1', remotePort: 0 };
    this.method = options.method || 'GET';
    this.path = options.path || '/';

    if (options.headers) {
      for (const [k, v] of Object.entries(options.headers)) {
        this._headers[String(k).toLowerCase()] = String(v);
      }
    }

    if (typeof callback === 'function') {
      this.once('response', callback);
    }
  }

  write(chunk) {
    if (!this._ended && !this._aborted) {
      this._body.push(
        __pi_http_is_binary_chunk(chunk)
          ? __pi_http_clone_body_chunk(chunk)
          : String(chunk)
      );
    }
    return true;
  }

  end(chunk, _encoding, callback) {
    if (typeof chunk === 'function') { callback = chunk; chunk = undefined; }
    if (typeof _encoding === 'function') { callback = _encoding; }
    if (chunk) this.write(chunk);
    if (typeof callback === 'function') this.once('finish', callback);

    this._ended = true;
    this._send();
    return this;
  }

  abort() {
    if (this._aborted) return;
    try { this.emit('abort'); } finally { this.destroy(); }
  }

  destroy(error) {
    if (this._aborted) return this;
    this._aborted = true;
    this.destroyed = true;
    this._clearTimeout();
    this._controller.abort(error);
    if (this._response) this._response.destroy();
    if (error) this.emit('error', error);
    this.emit('close');
    return this;
  }

  setTimeout(ms, callback) {
    if (typeof callback === 'function') this.once('timeout', callback);
    if (!Number.isFinite(Number(ms)) || Number(ms) < 0) throw new RangeError('Invalid HTTP timeout');
    this._timeoutMs = Number(ms);
    if (this._ended && !this._aborted) this._armTimeout();
    return this;
  }

  _clearTimeout() {
    if (this._timeoutId !== null) clearTimeout(this._timeoutId);
    this._timeoutId = null;
  }

  _armTimeout() {
    this._clearTimeout();
    if (!this._timeoutMs || this._aborted || (this._response && this._response.complete)) return;
    this._timeoutId = setTimeout(() => {
      const error = new Error('HTTP request timed out');
      error.code = 'ETIMEDOUT';
      try { this.emit('timeout'); } finally { this.destroy(error); }
    }, this._timeoutMs);
  }

  setNoDelay() { return this; }
  setSocketKeepAlive() { return this; }
  flushHeaders() {}
  getHeader(name) { return this._headers[String(name).toLowerCase()]; }
  setHeader(name, value) {
    if (!this._ended && !this._aborted) {
      this._headers[String(name).toLowerCase()] = String(value);
    }
    return this;
  }
  removeHeader(name) {
    if (!this._ended && !this._aborted) {
      delete this._headers[String(name).toLowerCase()];
    }
    return this;
  }

  _send() {
    if (this._aborted || this._sent) {
      return;
    }
    this._sent = true;

    const opts = this._options;
    const protocol = opts.protocol || 'http:';
    const hostname = opts.hostname || opts.host || 'localhost';
    const port = opts.port ? `:${opts.port}` : '';
    const path = opts.path || '/';
    const url = `${protocol}//${hostname}${port}${path}`;

    const headers = { ...this._headers };

    const method = (opts.method || 'GET').toUpperCase();
    const request = { url, method, headers };
    if (this._body.length > 0) {
      const hasBinaryChunk = this._body.some((chunk) => __pi_http_is_binary_chunk(chunk));
      if (hasBinaryChunk) {
        request.body_bytes = __pi_http_chunks_to_base64(this._body);
      } else {
        request.body = this._body.join('');
      }
    }
    // Reuse global fetch so requests and response streams share the same host
    // capability checks, cancellation, deadlines, and virtual loopback routing.
    if (typeof globalThis.pi === 'object' && typeof globalThis.pi.http === 'function') {
      try {
        if (opts.rejectUnauthorized === false || opts.ca !== undefined || opts.cert !== undefined ||
            opts.key !== undefined || opts.createConnection !== undefined || opts.lookup !== undefined || opts.socketPath !== undefined) {
          throw new Error('Custom HTTP sockets and TLS trust are not supported in PiJS; configure host networking instead');
        }
        const signal = opts.signal ? AbortSignal.any([this._controller.signal, opts.signal]) : this._controller.signal;
        this._armTimeout();
        const body = request.body_bytes !== undefined ? __pi_http_decode_body_bytes(request.body_bytes) : request.body;
        const fetchHttp = globalThis.__pi_fetch_for_node_http || globalThis.fetch;
        Promise.resolve(fetchHttp(url, { method, headers, body, signal, timeout: this._timeoutMs || undefined })).then(async (response) => {
          if (this._aborted) { if (response.body) await response.body.cancel(); return; }
          const responseHeaders = {};
          for (const [name, value] of response.headers.entries()) responseHeaders[name] = value;
          const incoming = new IncomingMessage(response.status, responseHeaders, '');
          incoming._cancelRequest = (reason) => this._controller.abort(reason);
          this._response = incoming;
          this._armTimeout();
          this.emit('response', incoming);
          await incoming._deliverWeb(response.body, () => this._armTimeout());
        }).catch((error) => {
          if (!this._aborted) this.emit('error', error instanceof Error ? error : new Error(String(error)));
        }).finally(() => this._clearTimeout());
      } catch (err) {
        this._clearTimeout();
        this.emit('error', err);
      }
    } else {
      // No pi.http available — emit error
      this.emit('error', new Error('HTTP requests require pi.http() hostcall'));
    }

    this.emit('finish');
  }

  _handleResponse(result) {
    if (this._aborted) {
      return;
    }

    if (!result || typeof result !== 'object') {
      this.emit('error', new Error('Invalid HTTP response from hostcall'));
      return;
    }

    const statusCode = result.status || result.statusCode || 200;
    const headers = result.headers || {};
    const body =
      result.body_bytes !== undefined && result.body_bytes !== null
        ? __pi_http_decode_body_bytes(result.body_bytes)
        : (result.body || result.data || '');

    const res = new IncomingMessage(statusCode, headers, body);
    this.emit('response', res);
    // Deliver body asynchronously (in next microtask)
    Promise.resolve().then(() => {
      if (!this._aborted) {
        res._deliver();
      }
    });
  }
}

// ─── Module API ──────────────────────────────────────────────────────────────

function _parseOptions(input, options) {
  if (typeof input === 'string') {
    try {
      const url = new URL(input);
      return {
        protocol: url.protocol,
        hostname: url.hostname,
        port: url.port || undefined,
        path: url.pathname + url.search,
        ...(options || {}),
      };
    } catch (_e) {
      return { path: input, ...(options || {}) };
    }
  }
  if (input && typeof input === 'object' && !(input instanceof URL)) {
    return { ...input };
  }
  if (input instanceof URL) {
    return {
      protocol: input.protocol,
      hostname: input.hostname,
      port: input.port || undefined,
      path: input.pathname + input.search,
      ...(options || {}),
    };
  }
  return options || {};
}

export function request(input, optionsOrCallback, callback) {
  let options;
  if (typeof optionsOrCallback === 'function') {
    callback = optionsOrCallback;
    options = _parseOptions(input);
  } else {
    options = _parseOptions(input, optionsOrCallback);
  }
  if (!options.protocol) options.protocol = 'http:';
  return new ClientRequest(options, callback);
}

export function get(input, optionsOrCallback, callback) {
  const req = request(input, optionsOrCallback, callback);
  req.end();
  return req;
}

// These servers are extension-owned in-memory routes. They never open a socket,
// and only the owning extension and its explicitly registered providers can
// dispatch requests to them. Every outbound request still uses the host bridge.
const VIRTUAL_HTTP_QUEUE_LIMIT = 1024 * 1024;
const VIRTUAL_HTTP_HIGH_WATER_MARK = 64 * 1024;
const VIRTUAL_HTTP_REQUEST_LIMIT = 10 * 1024 * 1024;

function virtualHttpError(message, code = 'ERR_PIJS_VIRTUAL_HTTP') {
  const error = new Error(message);
  error.code = code;
  return error;
}

function virtualHttpHeader(name, value) {
  const key = String(name).toLowerCase();
  const text = Array.isArray(value) ? value.map(String).join(', ') : String(value);
  if (!/^[!#$%&'*+.^_`|~0-9a-z-]+$/.test(key) || /[\r\n\0]/.test(text)) {
    throw virtualHttpError('Invalid virtual HTTP header', 'ERR_INVALID_HTTP_TOKEN');
  }
  return [key, text];
}

function makeVirtualTransaction(server, request) {
  const url = new URL(request.url);
  const body = request.body_bytes !== undefined
    ? __pi_http_decode_body_bytes(request.body_bytes)
    : __pi_http_clone_body_chunk(request.body || '');
  if (body.length > VIRTUAL_HTTP_REQUEST_LIMIT) {
    throw virtualHttpError('Virtual HTTP request exceeds 10 MiB');
  }
  const req = new EventEmitter();
  req.method = String(request.method || 'GET').toUpperCase();
  req.url = url.pathname + url.search;
  req.headers = Object.create(null);
  for (const [name, value] of Object.entries(request.headers || {})) {
    const [key, text] = virtualHttpHeader(name, value);
    req.headers[key] = text;
  }
  req.headers.host = url.host;
  req.httpVersion = '1.1';
  req.complete = false;
  req.aborted = false;
  req.destroyed = false;
  req.socket = new EventEmitter();
  req.socket.remoteAddress = url.hostname === '[::1]' ? '::1' : '127.0.0.1';
  req.socket.localAddress = req.socket.remoteAddress;
  req.socket.localPort = Number(url.port || 80);
  req.socket.setNoDelay = () => req.socket;
  req.socket.setKeepAlive = () => req.socket;
  req.connection = req.socket;
  req.setEncoding = (encoding) => { req._encoding = encoding; return req; };
  req.pause = () => req;
  req.resume = () => req;

  const res = new EventEmitter();
  res.statusCode = 200;
  res.headersSent = false;
  res.writableEnded = false;
  res.writableFinished = false;
  res.destroyed = false;
  res.socket = req.socket;
  const headers = Object.create(null);
  const queue = [];
  let queuedBytes = 0;
  let backpressured = false;
  let head = null;
  let headDelivered = false;
  let failure = null;
  let finished = false;
  let waiting = null;
  let timeoutMs = 0;
  let timeoutHandle = null;
  let finish;
  const finishedPromise = new Promise((resolve) => { finish = resolve; });
  const wake = () => {
    if (waiting) { const resolve = waiting; waiting = null; resolve(); }
  };
  const complete = () => {
    if (finished) return;
    finished = true;
    if (timeoutHandle !== null) clearTimeout(timeoutHandle);
    timeoutHandle = null;
    wake();
    finish();
  };
  const fail = (reason) => {
    if (res.destroyed) return;
    if (finished && res.writableEnded) {
      queue.length = 0;
      queuedBytes = 0;
      return;
    }
    failure = reason instanceof Error ? reason : virtualHttpError(String(reason || 'Virtual HTTP request cancelled'), 'ABORT_ERR');
    queue.length = 0;
    queuedBytes = 0;
    req.aborted = !req.complete;
    req.destroyed = true;
    res.destroyed = true;
    // Notify every cancellation observer even if one listener throws.
    for (const [emitter, event, value] of [
      [req, 'aborted', undefined], [req, 'close', undefined],
      [res, 'close', undefined], [req.socket, 'close', undefined],
    ]) {
      try { emitter.emit(event, value); } catch (_) {}
    }
    complete();
  };
  const armTimeout = () => {
    if (timeoutHandle !== null) clearTimeout(timeoutHandle);
    timeoutHandle = null;
    if (!timeoutMs || finished) return;
    timeoutHandle = setTimeout(() => {
      timeoutHandle = null;
      try { res.emit('timeout'); } catch (error) { fail(error); }
      if (!finished && timeoutHandle === null) {
        fail(virtualHttpError('Virtual HTTP request timed out', 'ETIMEDOUT'));
      }
    }, timeoutMs);
  };
  req.destroy = (err) => { fail(err); return req; };
  req.socket.destroy = (err) => { fail(err); return req.socket; };
  res.destroy = (err) => { fail(err); return res; };
  res.setHeader = (name, value) => {
    if (res.headersSent) throw virtualHttpError('Headers already sent', 'ERR_HTTP_HEADERS_SENT');
    const [key, text] = virtualHttpHeader(name, value);
    if (!(key in headers) && Object.keys(headers).length >= 100) {
      throw virtualHttpError('Too many virtual HTTP response headers');
    }
    headers[key] = text;
    return res;
  };
  res.getHeader = (name) => headers[String(name).toLowerCase()];
  res.getHeaders = () => ({ ...headers });
  res.hasHeader = (name) => Object.prototype.hasOwnProperty.call(headers, String(name).toLowerCase());
  res.removeHeader = (name) => {
    if (res.headersSent) throw virtualHttpError('Headers already sent', 'ERR_HTTP_HEADERS_SENT');
    delete headers[String(name).toLowerCase()];
  };
  res.flushHeaders = () => {
    if (res.destroyed || res.headersSent) return;
    const status = Number(res.statusCode);
    if (!Number.isInteger(status) || status < 100 || status > 599) {
      throw virtualHttpError('Invalid virtual HTTP response status');
    }
    res.headersSent = true;
    head = { status, headers: { ...headers } };
    wake();
  };
  res.writeHead = (status, reasonOrHeaders, maybeHeaders) => {
    if (res.headersSent) throw virtualHttpError('Headers already sent', 'ERR_HTTP_HEADERS_SENT');
    res.statusCode = status;
    const values = typeof reasonOrHeaders === 'string' ? maybeHeaders : reasonOrHeaders;
    for (const [name, value] of Object.entries(values || {})) res.setHeader(name, value);
    res.flushHeaders();
    return res;
  };
  res.write = (chunk, encoding, callback) => {
    const cb = typeof encoding === 'function' ? encoding : callback;
    if (res.destroyed) return false;
    if (res.writableEnded) throw virtualHttpError('Write after end', 'ERR_STREAM_WRITE_AFTER_END');
    res.flushHeaders();
    const bytes = typeof chunk === 'string' && typeof Buffer !== 'undefined'
      ? Buffer.from(chunk, typeof encoding === 'string' ? encoding : 'utf8')
      : __pi_http_clone_body_chunk(chunk);
    if (queuedBytes + bytes.length > VIRTUAL_HTTP_QUEUE_LIMIT) {
      fail(virtualHttpError('Virtual HTTP response buffer exceeds 1 MiB'));
      return false;
    }
    if (bytes.length > 0) {
      queue.push(bytes);
      queuedBytes += bytes.length;
      armTimeout();
      wake();
    }
    if (typeof cb === 'function') queueMicrotask(() => cb());
    backpressured = queuedBytes >= VIRTUAL_HTTP_HIGH_WATER_MARK;
    return !backpressured;
  };
  res.end = (chunk, encoding, callback) => {
    const cb = typeof chunk === 'function' ? chunk : typeof encoding === 'function' ? encoding : callback;
    if (res.destroyed || res.writableEnded) return res;
    if (chunk !== undefined && chunk !== null && typeof chunk !== 'function') res.write(chunk, encoding);
    if (res.destroyed) return res;
    res.flushHeaders();
    res.writableEnded = true;
    res.writableFinished = true;
    try { res.emit('finish'); } finally {
      try { res.emit('close'); } finally { complete(); }
    }
    if (typeof cb === 'function') queueMicrotask(() => cb());
    return res;
  };
  res.setTimeout = (ms, callback) => {
    if (!Number.isFinite(Number(ms)) || Number(ms) < 0) throw new RangeError('Invalid virtual HTTP timeout');
    if (typeof callback === 'function') res.once('timeout', callback);
    timeoutMs = Number(ms);
    armTimeout();
    return res;
  };
  req.setTimeout = (ms, callback) => { res.setTimeout(ms, callback); return req; };
  req.socket.setTimeout = (ms, callback) => { res.setTimeout(ms, callback); return req.socket; };
  const poll = () => {
    if (failure) throw failure;
    if (head && !headDelivered) {
      headDelivered = true;
      return { done: false, value: head };
    }
    if (queue.length > 0) {
      const bytes = queue.shift();
      queuedBytes -= bytes.length;
      armTimeout();
      if (backpressured && queuedBytes < VIRTUAL_HTTP_HIGH_WATER_MARK) {
        backpressured = false;
        queueMicrotask(() => { if (!res.destroyed) res.emit('drain'); });
      }
      return { done: false, value: { body_bytes: __pi_http_chunks_to_base64([bytes]) } };
    }
    return finished ? { done: true } : { done: false, value: { pending: true } };
  };
  // The callback and request body are delivered after callers own a cancellation
  // handle. This also gives standard Node request listeners time to attach.
  queueMicrotask(() => {
    if (res.destroyed) return;
    try {
      server.emit('request', req, res);
      if (!req.destroyed && !res.writableEnded) {
        if (body.length > 0) req.emit('data', __pi_http_decode_chunk(body, req._encoding));
        req.complete = true;
        req.emit('end');
      }
    } catch (error) { fail(error); }
  });
  return {
    finished: finishedPromise,
    poll,
    async next() {
      for (;;) {
        const result = poll();
        if (!result.value || !result.value.pending) return result;
        await new Promise((resolve) => { waiting = resolve; });
      }
    },
    cancel(reason) { fail(reason); return { done: true }; },
  };
}

class Server extends EventEmitter {
  constructor(listener) {
    super();
    this._binding = null;
    this.listening = false;
    this.on('request', (req, res) => {
      try { Promise.resolve(listener.call(this, req, res)).catch((error) => res.destroy(error)); }
      catch (error) { res.destroy(error); }
    });
  }
  listen(portOrOptions, hostOrCallback, callback) {
    if (this.listening) throw virtualHttpError('Server is already listening', 'ERR_SERVER_ALREADY_LISTEN');
    const options = portOrOptions && typeof portOrOptions === 'object' ? portOrOptions : { port: portOrOptions, host: hostOrCallback };
    const cb = typeof hostOrCallback === 'function' ? hostOrCallback : callback;
    this._binding = globalThis.__pi_virtual_http_listen(options.host, options.port, (request) => makeVirtualTransaction(this, request), () => {
      this.listening = false;
      this._binding = null;
      this.emit('close');
    });
    this.listening = true;
    queueMicrotask(() => {
      if (!this.listening) return;
      this.emit('listening');
      if (typeof cb === 'function') cb();
    });
    return this;
  }
  address() {
    return this._binding ? { address: this._binding.host, family: this._binding.host === '::1' ? 'IPv6' : 'IPv4', port: this._binding.port } : null;
  }
  close(callback) {
    if (typeof callback === 'function') this.once('close', callback);
    if (this._binding) this._binding.close();
    else if (typeof callback === 'function') queueMicrotask(() => callback());
    return this;
  }
  closeAllConnections() { if (this._binding) this._binding.cancelRequests(); }
  closeIdleConnections() {}
  ref() { return this; }
  unref() { return this; }
}

export function createServer(optionsOrListener, listener) {
  const callback = typeof optionsOrListener === 'function' ? optionsOrListener : listener;
  if (typeof callback !== 'function') {
    throw new Error('node:http.createServer is not available without an explicit request listener in PiJS');
  }
  return new Server(callback);
}

export { STATUS_CODES, METHODS, IncomingMessage, ClientRequest, Server };
export default { request, get, createServer, STATUS_CODES, METHODS, IncomingMessage, ClientRequest, Server };
"#;

/// The JS source for the `node:https` virtual module.
pub const NODE_HTTPS_JS: &str = r#"
import EventEmitter from "node:events";
import * as http from "node:http";

export function request(input, optionsOrCallback, callback) {
  let options;
  if (typeof optionsOrCallback === 'function') {
    callback = optionsOrCallback;
    options = typeof input === 'string' || input instanceof URL
      ? { ...(typeof input === 'string' ? (() => { try { const u = new URL(input); return { protocol: u.protocol, hostname: u.hostname, port: u.port, path: u.pathname + u.search }; } catch(_) { return { path: input }; } })() : { protocol: input.protocol, hostname: input.hostname, port: input.port, path: input.pathname + input.search }) }
      : { ...(input || {}) };
  } else {
    options = typeof input === 'string' || input instanceof URL
      ? { ...(typeof input === 'string' ? (() => { try { const u = new URL(input); return { protocol: u.protocol, hostname: u.hostname, port: u.port, path: u.pathname + u.search }; } catch(_) { return { path: input }; } })() : { protocol: input.protocol, hostname: input.hostname, port: input.port, path: input.pathname + input.search }), ...(optionsOrCallback || {}) }
      : { ...(input || {}), ...(optionsOrCallback || {}) };
  }
  if (!options.protocol) options.protocol = 'https:';
  return http.request(options, callback);
}

export function get(input, optionsOrCallback, callback) {
  const req = request(input, optionsOrCallback, callback);
  req.end();
  return req;
}

export function createServer() {
  throw new Error('node:https.createServer is not available in PiJS');
}

export const globalAgent = {};

export default { request, get, createServer, globalAgent };
"#;
