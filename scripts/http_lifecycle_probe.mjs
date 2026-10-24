#!/usr/bin/env node
// Diagnostic only: execute the exact embedded node:http JS and the same cases
// as the QuickJS Rust regression. This is not a DSR or native-runtime test pass.
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve } from 'node:path';
import vm from 'node:vm';

const root = resolve(process.argv[2] || '.');
const catchingEmitter = process.argv.includes('--catching-emitter');
const shim = readFileSync(resolve(root, 'src/http_shim.rs'), 'utf8');
const tests = readFileSync(resolve(root, 'tests/node_http_lifecycle.rs'), 'utf8');
function rawConstant(text, name) {
  for (const hashes of ['#', '']) {
    const marker = `const ${name}: &str = r${hashes}"`;
    const start = text.indexOf(marker);
    if (start < 0) continue;
    const from = start + marker.length;
    const end = text.indexOf(`"${hashes};`, from);
    if (end < 0) throw new Error(`Unterminated ${name}`);
    return text.slice(from, end);
  }
  throw new Error(`Missing ${name}`);
}
function stringConstant(text, name) {
  const match = text.match(new RegExp(`const ${name}: &str = "([^"]*)";`));
  if (!match) throw new Error(`Missing ${name}`);
  return match[1];
}
const source = rawConstant(shim, 'NODE_HTTP_JS');
const cases = rawConstant(tests, 'LIFECYCLE_CASES_JS');
// The cases replace the transport through the same one-expression
// substitution as the Rust regression (TRANSPORT_LOOKUP there).
const transportLookup = stringConstant(tests, 'TRANSPORT_LOOKUP');
if (source.split(transportLookup).length !== 2) throw new Error('Transport lookup not found exactly once');
const caseSource = source.replace(transportLookup, stringConstant(tests, 'CASE_TRANSPORT'));
globalThis.pi = { http() { throw new Error('Unexpected native hostcall in diagnostic'); } };
const moduleUrl = text => `data:text/javascript;base64,${Buffer.from(text).toString('base64')}`;
// A separate sensitivity check models PiJS's documented catch-and-continue
// EventEmitter behavior. This is not its source or proof of QuickJS execution.
const emitterModel = `
import EventEmitter from 'node:events';
export default class CatchingEmitter extends EventEmitter {
  emit(event, ...args) {
    const listeners = this.rawListeners(event).slice();
    for (const listener of listeners) {
      try { Reflect.apply(listener, this, args); }
      catch (error) { if (event !== 'error') this.emit('error', error); }
    }
    return listeners.length > 0;
  }
}`;
const executable = catchingEmitter
  ? caseSource.replace('import EventEmitter from "node:events";', `import EventEmitter from ${JSON.stringify(moduleUrl(emitterModel))};`)
  : caseSource;
if (catchingEmitter && executable === caseSource) throw new Error('EventEmitter import not found');
const http = await import(moduleUrl(executable));
vm.runInThisContext(cases, { filename: 'node_http_lifecycle.rs:shared-cases' });
const results = await globalThis.__httpLifecycleCases(http);
console.log(JSON.stringify({
  engine: process.version,
  emitter: catchingEmitter ? 'catch-and-continue model (not native PiJS)' : 'Node.js',
  shimSha256: createHash('sha256').update(source).digest('hex'),
  casesSha256: createHash('sha256').update(cases).digest('hex'),
  passed: results.filter(result => result.passed).length,
  total: results.length,
  results,
}, null, 2));
process.exitCode = results.every(result => result.passed) ? 0 : 1;
