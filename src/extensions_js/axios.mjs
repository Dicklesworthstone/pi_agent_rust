// Axios-shaped HTTP for imported extensions. All I/O uses PiJS fetch, which
// owns capability checks, response budgets, streaming credits and cancellation.
// This adapter never creates an independent socket, agent or credential store.
const methods = ['delete', 'get', 'head', 'options', 'post', 'put', 'patch'];
const unsafeKeys = new Set(['__proto__', 'prototype', 'constructor']);
const ownEntries = value => Object.entries(value || {}).filter(([key]) => !unsafeKeys.has(key));
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const plainObject = value => object(value) &&
  (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);

export class AxiosError extends Error {
  constructor(message, code, config, request, response) {
    super(message || 'AxiosError');
    this.name = 'AxiosError';
    this.isAxiosError = true;
    this.code = code;
    this.config = config;
    this.request = request;
    this.response = response;
    this.status = response ? response.status : undefined;
  }
  toJSON() {
    return { message: this.message, name: this.name, code: this.code,
      status: this.status, config: this.config };
  }
  static from(error, code, config, request, response) {
    const wrapped = new AxiosError(error && error.message || String(error),
      code || error && error.code, config, request, response);
    wrapped.cause = error;
    return wrapped;
  }
}
for (const code of ['ERR_BAD_OPTION_VALUE', 'ECONNABORTED', 'ETIMEDOUT',
  'ERR_NETWORK', 'ERR_BAD_RESPONSE', 'ERR_BAD_REQUEST', 'ERR_CANCELED',
  'ERR_NOT_SUPPORT', 'ERR_INVALID_URL']) AxiosError[code] = code;

export class CanceledError extends AxiosError {
  constructor(message, config, request) {
    super(message || 'canceled', 'ERR_CANCELED', config, request);
    this.name = 'CanceledError';
    this.__CANCEL__ = true;
  }
}
export const Cancel = CanceledError;
export const isCancel = value => !!(value && value.__CANCEL__);
export const isAxiosError = value => !!(value && value.isAxiosError === true);

export class CancelToken {
  constructor(executor) {
    if (typeof executor !== 'function') throw new TypeError('CancelToken executor must be a function');
    this._listeners = new Set();
    let resolve;
    this.promise = new Promise(done => { resolve = done; });
    executor(message => {
      if (this.reason) return;
      this.reason = new CanceledError(message);
      resolve(this.reason);
      const listeners = Array.from(this._listeners);
      this._listeners.clear();
      for (const listener of listeners) listener(this.reason);
    });
  }
  throwIfRequested() { if (this.reason) throw this.reason; }
  subscribe(listener) {
    if (this.reason) listener(this.reason);
    else this._listeners.add(listener);
  }
  unsubscribe(listener) { this._listeners.delete(listener); }
  toAbortSignal() {
    const controller = new AbortController();
    const listener = reason => controller.abort(reason);
    this.subscribe(listener);
    controller.signal.unsubscribe = () => this.unsubscribe(listener);
    return controller.signal;
  }
  static source() {
    let cancel;
    const token = new CancelToken(value => { cancel = value; });
    return { token, cancel };
  }
}

export class AxiosHeaders {
  constructor(headers) {
    if (!headers) return;
    const entries = headers instanceof AxiosHeaders ? ownEntries(headers)
      : typeof headers.toJSON === 'function' ? ownEntries(headers.toJSON())
      : typeof headers.entries === 'function' ? Array.from(headers.entries()) : ownEntries(headers);
    for (const [name, value] of entries) this.set(name, value);
  }
  set(name, value, rewrite = true) {
    if (object(name)) {
      for (const [key, entry] of ownEntries(name)) this.set(key, entry, value);
      return this;
    }
    const key = String(name).trim().toLowerCase();
    if (!key || unsafeKeys.has(key)) throw new TypeError('Invalid HTTP header name');
    if (rewrite === false && this.has(key)) return this;
    for (const existing of Object.keys(this)) {
      if (existing.toLowerCase() === key) delete this[existing];
    }
    // Header names such as "get" must not shadow this facade's methods.
    const stored = typeof AxiosHeaders.prototype[key] === 'function'
      ? key[0].toUpperCase() + key.slice(1) : key;
    this[stored] = value;
    return this;
  }
  get(name) {
    const key = String(name).toLowerCase();
    const actual = Object.keys(this).reverse().find(entry => entry.toLowerCase() === key);
    return actual === undefined ? undefined : this[actual];
  }
  has(name) { return this.get(name) !== undefined; }
  delete(name) {
    const key = String(name).toLowerCase();
    let removed = false;
    for (const actual of Object.keys(this)) {
      if (actual.toLowerCase() === key) { delete this[actual]; removed = true; }
    }
    return removed;
  }
  clear() { for (const key of Object.keys(this)) delete this[key]; }
  toJSON() {
    const result = Object.create(null);
    for (const [key, value] of ownEntries(this)) {
      if (value === undefined || value === null || value === false) continue;
      result[key.toLowerCase()] = Array.isArray(value) ? value.join(', ') : String(value);
    }
    return result;
  }
  [Symbol.iterator]() { return Object.entries(this.toJSON())[Symbol.iterator](); }
  static from(value) { return value instanceof AxiosHeaders ? value : new AxiosHeaders(value); }
  static concat(...values) {
    const result = new AxiosHeaders();
    for (const value of values) result.set(new AxiosHeaders(value));
    return result;
  }
}

function mergeHeaders(base, override) {
  const result = Object.create(null);
  for (const source of [base, override]) {
    const values = source;
    for (const [key, value] of ownEntries(values)) {
      const normalized = key.toLowerCase();
      if ((normalized === 'common' || methods.includes(normalized)) && object(value)) {
        result[normalized] = mergeHeaders(result[normalized], value);
      } else {
        for (const prior of Object.keys(result)) {
          if (prior.toLowerCase() === normalized) delete result[prior];
        }
        result[key] = value;
      }
    }
  }
  return result;
}

export function mergeConfig(base = {}, override = {}) {
  const result = Object.create(null);
  for (const source of [base, override]) {
    for (const [key, value] of ownEntries(source)) {
      if (value !== undefined) result[key] = value;
    }
  }
  result.headers = mergeHeaders(base.headers, override.headers);
  for (const key of ['params', 'transitional']) {
    if (plainObject(base[key]) && plainObject(override[key])) {
      result[key] = Object.assign(Object.create(null),
        Object.fromEntries(ownEntries(base[key])), Object.fromEntries(ownEntries(override[key])));
    }
  }
  return result;
}

function flattenHeaders(config) {
  const raw = config.headers || {};
  const result = new AxiosHeaders();
  for (const layer of [raw.common, raw[config.method]]) {
    for (const [key, value] of ownEntries(layer)) result.set(key, value);
  }
  for (const [key, value] of ownEntries(raw)) {
    if (!object(value) || (key.toLowerCase() !== 'common' && !methods.includes(key.toLowerCase()))) {
      result.set(key, value);
    }
  }
  return result;
}

function badOption(config, message, code = 'ERR_BAD_OPTION_VALUE') {
  return new AxiosError(message, code, config);
}

function requestURL(config) {
  let url = config.url === undefined ? '' : String(config.url);
  const absolute = /^(?:[a-z][a-z\d+.-]*:)?\/\//i.test(url);
  if (config.baseURL && (!absolute || config.allowAbsoluteUrls === false)) {
    url = String(config.baseURL).replace(/\/+$/, '') + '/' + url.replace(/^\/+/, '');
  }
  if (!/^https?:\/\//i.test(url)) throw badOption(config, 'Axios requires an absolute HTTP(S) URL', 'ERR_INVALID_URL');
  let parsed;
  try { parsed = new URL(url); }
  catch (error) { throw AxiosError.from(error, 'ERR_INVALID_URL', config); }
  if (parsed.username || parsed.password) {
    throw badOption(config, 'URL credentials are unsupported; use auth or explicit headers', 'ERR_NOT_SUPPORT');
  }
  parsed.hash = '';
  const params = config.params;
  if (params !== undefined && params !== null) {
    let serialized;
    const serializer = config.paramsSerializer;
    if (typeof serializer === 'function') serialized = serializer(params);
    else if (serializer && typeof serializer.serialize === 'function') serialized = serializer.serialize(params, serializer);
    else if (params instanceof URLSearchParams) serialized = params.toString();
    else {
      const query = [];
      const encode = serializer && serializer.encode || encodeURIComponent;
      if (typeof encode !== 'function') throw badOption(config, 'paramsSerializer.encode must be a function');
      const emit = (key, value) => {
        if (value === undefined || value === null) return;
        const text = value instanceof Date ? value.toISOString()
          : object(value) ? JSON.stringify(value) : String(value);
        query.push(encode(key) + '=' + encode(text));
      };
      for (const [key, value] of ownEntries(params)) {
        if (Array.isArray(value)) value.forEach((item, index) => emit(
          serializer && serializer.indexes === true ? key + '[' + index + ']'
            : serializer && serializer.indexes === null ? key : key + '[]', item));
        else emit(key, value);
      }
      serialized = query.join('&');
    }
    if (typeof serialized !== 'string') throw badOption(config, 'paramsSerializer must return a string');
    if (serialized) parsed.search += (parsed.search ? '&' : '?') + serialized.replace(/^\?/, '');
  }
  return parsed.href;
}

function transform(value, transforms, headers, config, status) {
  for (const callback of Array.isArray(transforms) ? transforms : [transforms]) {
    if (typeof callback !== 'function') throw badOption(config, 'Transforms must be functions');
    value = callback.call(config, value, headers, status);
    if (value && typeof value.then === 'function') throw badOption(config, 'Axios transforms must be synchronous');
  }
  return value;
}

function formBody(data) {
  const params = new URLSearchParams();
  const active = new Set();
  const visit = (key, value) => {
    if (value === undefined || value === null) return;
    if (value instanceof Date) { params.append(key, value.toISOString()); return; }
    if (typeof value !== 'object') { params.append(key, String(value)); return; }
    if (active.has(value)) throw new TypeError('Circular form data');
    active.add(value);
    if (Array.isArray(value)) {
      value.forEach((entry, index) => visit(key + '[' + (object(entry) || Array.isArray(entry) ? index : '') + ']', entry));
    } else {
      for (const [name, entry] of ownEntries(value)) visit(key ? key + '[' + name + ']' : name, entry);
    }
    active.delete(value);
  };
  for (const [key, value] of ownEntries(data)) visit(key, value);
  return params.toString();
}

function requestBody(config) {
  let body = config.data;
  const headers = config.headers;
  if (config.transformRequest !== undefined) body = transform(body, config.transformRequest, headers, config);
  else if (body instanceof URLSearchParams) {
    if (!headers.has('content-type')) headers.set('content-type', 'application/x-www-form-urlencoded;charset=utf-8');
    body = body.toString();
  } else if (Array.isArray(body) || (object(body) &&
    (Object.getPrototypeOf(body) === Object.prototype || Object.getPrototypeOf(body) === null))) {
    if (String(headers.get('content-type') || '').toLowerCase().includes('application/x-www-form-urlencoded')) {
      body = formBody(body);
    } else {
      if (!headers.has('content-type')) headers.set('content-type', 'application/json');
      body = JSON.stringify(body);
    }
  }
  if (body === undefined || body === null) return undefined;
  const bytes = body instanceof ArrayBuffer ? new Uint8Array(body)
    : ArrayBuffer.isView(body) ? new Uint8Array(body.buffer, body.byteOffset, body.byteLength)
    : typeof body === 'string' ? new TextEncoder().encode(body) : null;
  if (!bytes) throw badOption(config, 'PiJS Axios accepts text, JSON, URLSearchParams and binary request bodies', 'ERR_NOT_SUPPORT');
  if (config.method === 'get' || config.method === 'head') throw badOption(config, 'GET and HEAD request bodies are unsupported', 'ERR_NOT_SUPPORT');
  if (config.maxBodyLength >= 0 && bytes.byteLength > config.maxBodyLength) {
    throw badOption(config, 'Request body exceeds maxBodyLength', 'ERR_BAD_REQUEST');
  }
  return body;
}

async function readBody(response, config) {
  if (!response.body) return new Uint8Array();
  const reader = response.body.getReader();
  const chunks = [];
  let length = 0;
  try {
    while (true) {
      const next = await reader.read();
      if (next.done) break;
      const bytes = next.value instanceof Uint8Array ? next.value : new Uint8Array(next.value);
      length += bytes.byteLength;
      if (config.maxContentLength >= 0 && length > config.maxContentLength) {
        throw badOption(config, 'Response body exceeds maxContentLength', 'ERR_BAD_RESPONSE');
      }
      chunks.push(bytes);
    }
  } catch (error) {
    try { await reader.cancel(error); } catch (_) {}
    throw error;
  } finally { reader.releaseLock(); }
  const result = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) { result.set(chunk, offset); offset += chunk.byteLength; }
  return result;
}

async function dispatch(config) {
  if (!object(config)) throw new AxiosError('Request interceptor must return a config object', 'ERR_BAD_OPTION_VALUE');
  config.method = String(config.method || 'get').toLowerCase();
  config.headers = flattenHeaders(config);
  for (const key of ['httpAgent', 'httpsAgent', 'proxy', 'socketPath', 'transport',
    'beforeRedirect', 'maxRedirects', 'decompress', 'insecureHTTPParser', 'lookup',
    'onUploadProgress', 'onDownloadProgress']) {
    if (config[key] !== undefined && config[key] !== null) {
      throw badOption(config, 'PiJS Axios does not support ' + key + '; HTTP transport is host-owned', 'ERR_NOT_SUPPORT');
    }
  }
  if (config.withCredentials === true) throw badOption(config, 'PiJS Axios has no ambient cookie jar', 'ERR_NOT_SUPPORT');
  if (config.adapter !== undefined && config.adapter !== 'fetch') throw badOption(config, 'Only the PiJS fetch adapter is supported', 'ERR_NOT_SUPPORT');
  if (config.responseType && !['json', 'text', 'arraybuffer'].includes(config.responseType)) {
    throw badOption(config, 'Unsupported PiJS Axios responseType: ' + config.responseType, 'ERR_NOT_SUPPORT');
  }
  if (config.responseEncoding && !['utf8', 'utf-8'].includes(config.responseEncoding.toLowerCase())) {
    throw badOption(config, 'PiJS Axios text responses use UTF-8', 'ERR_NOT_SUPPORT');
  }
  const timeout = config.timeout === undefined ? 0 : Number(config.timeout);
  if (!Number.isFinite(timeout) || timeout < 0 || timeout > 2147483647) throw badOption(config, 'timeout must be a nonnegative finite millisecond value');
  for (const key of ['maxBodyLength', 'maxContentLength']) {
    if (config[key] !== undefined && (!Number.isSafeInteger(config[key]) || config[key] < -1)) {
      throw badOption(config, key + ' must be -1 or a nonnegative safe integer');
    }
  }
  if (config.validateStatus !== undefined && config.validateStatus !== null && typeof config.validateStatus !== 'function') {
    throw badOption(config, 'validateStatus must be a function or null');
  }
  const signal = config.signal;
  if (signal && (typeof signal.aborted !== 'boolean' ||
    typeof signal.addEventListener !== 'function' || typeof signal.removeEventListener !== 'function')) {
    throw badOption(config, 'signal must be an AbortSignal');
  }
  const token = config.cancelToken;
  if (token && !(token instanceof CancelToken)) throw badOption(config, 'cancelToken must be an Axios CancelToken');
  if (token) token.throwIfRequested();
  if (signal && signal.aborted) throw new CanceledError(undefined, config);
  const url = requestURL(config);
  if (config.auth) {
    config.headers.set('authorization', 'Basic ' + Buffer.from(
      String(config.auth.username || '') + ':' + String(config.auth.password || ''), 'utf8').toString('base64'));
  }
  const body = requestBody(config);
  const request = { url, method: config.method.toUpperCase() };
  const controller = new AbortController();
  let timer;
  const abort = reason => { if (!controller.signal.aborted) controller.abort(reason); };
  const onSignal = () => { if (signal.aborted) abort(new CanceledError(undefined, config, request)); };
  const onToken = reason => abort(new CanceledError(reason.message, config, request));
  if (signal) signal.addEventListener('abort', onSignal);
  if (token) token.subscribe(onToken);
  // URL/header/body getters and request transforms can abort synchronously.
  if (signal && signal.aborted) onSignal();
  if (timeout > 0) {
    timer = setTimeout(() => abort(new AxiosError(config.timeoutErrorMessage ||
      'timeout of ' + timeout + 'ms exceeded', config.transitional &&
      config.transitional.clarifyTimeoutError ? 'ETIMEDOUT' : 'ECONNABORTED', config, request)), timeout);
  }
  try {
    if (controller.signal.aborted) throw controller.signal.reason;
    const native = await globalThis.fetch(url, {
      method: request.method, headers: config.headers.toJSON(), body, signal: controller.signal,
    });
    const bytes = await readBody(native, config);
    if (controller.signal.aborted) throw controller.signal.reason;
    const response = { data: config.responseType === 'arraybuffer' ? bytes.buffer : new TextDecoder().decode(bytes),
      status: native.status, statusText: native.statusText, headers: new AxiosHeaders(native.headers),
      config, request };
    if (config.transformResponse !== undefined) {
      response.data = transform(response.data, config.transformResponse, response.headers, config, response.status);
    } else if (typeof response.data === 'string' && response.data &&
      (config.responseType === 'json' || (!config.responseType &&
        (!config.transitional || config.transitional.forcedJSONParsing !== false)))) {
      try { response.data = JSON.parse(response.data); }
      catch (error) {
        if (config.responseType === 'json' && config.transitional && config.transitional.silentJSONParsing === false) {
          throw AxiosError.from(error, 'ERR_BAD_RESPONSE', config, request, response);
        }
      }
    }
    if (config.validateStatus && !config.validateStatus(response.status)) {
      throw new AxiosError('Request failed with status code ' + response.status,
        response.status >= 500 ? 'ERR_BAD_RESPONSE' : 'ERR_BAD_REQUEST', config, request, response);
    }
    return response;
  } catch (error) {
    if (controller.signal.aborted) throw controller.signal.reason;
    if (isAxiosError(error)) throw error;
    throw AxiosError.from(error, error && error.code || 'ERR_NETWORK', config, request);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
    if (signal) signal.removeEventListener('abort', onSignal);
    if (token) token.unsubscribe(onToken);
  }
}

class InterceptorManager {
  constructor() { this._handlers = new Map(); this._next = 0; }
  use(fulfilled, rejected, options = {}) {
    if (options.synchronous) throw new AxiosError('PiJS Axios uses asynchronous interceptors', 'ERR_NOT_SUPPORT');
    const id = this._next++;
    this._handlers.set(id, { fulfilled, rejected, runWhen: options.runWhen });
    return id;
  }
  eject(id) { this._handlers.delete(id); }
  clear() { this._handlers.clear(); }
}

function createInstance(defaults) {
  const instance = (urlOrConfig = {}, options = {}) => {
    const input = typeof urlOrConfig === 'string' || urlOrConfig instanceof URL
      ? { ...options, url: String(urlOrConfig) } : urlOrConfig;
    let config;
    try { config = mergeConfig(instance.defaults, input); }
    catch (error) { return Promise.reject(error); }
    config.method = String(config.method || 'get').toLowerCase();
    config.headers = flattenHeaders(config);
    let chain = Promise.resolve(config);
    try {
      for (const handler of Array.from(instance.interceptors.request._handlers.values()).reverse()) {
        if (handler.runWhen && handler.runWhen(config) === false) continue;
        chain = chain.then(handler.fulfilled, handler.rejected);
      }
    } catch (error) { chain = Promise.reject(error); }
    chain = chain.then(dispatch);
    for (const handler of instance.interceptors.response._handlers.values()) {
      chain = chain.then(handler.fulfilled, handler.rejected);
    }
    return chain;
  };
  instance.defaults = defaults;
  instance.interceptors = { request: new InterceptorManager(), response: new InterceptorManager() };
  instance.request = instance;
  instance.getUri = config => requestURL(mergeConfig(instance.defaults, config || {}));
  instance.create = options => createInstance(mergeConfig(instance.defaults, options || {}));
  for (const method of ['get', 'delete', 'head', 'options']) {
    instance[method] = (url, config) => instance({ ...config, url, method });
  }
  for (const method of ['post', 'put', 'patch']) {
    instance[method] = (url, data, config) => instance({ ...config, url, method, data });
  }
  Object.assign(instance, { AxiosError, AxiosHeaders, CanceledError, Cancel, CancelToken,
    isAxiosError, isCancel, mergeConfig, all: Promise.all.bind(Promise), spread: callback => values => callback(...values) });
  return instance;
}

const axios = createInstance({
  timeout: 0, maxBodyLength: -1, maxContentLength: -1,
  headers: { common: { Accept: 'application/json, text/plain, */*' } },
  validateStatus: status => status >= 200 && status < 300,
});
export const all = axios.all;
export const spread = axios.spread;
export default axios;
