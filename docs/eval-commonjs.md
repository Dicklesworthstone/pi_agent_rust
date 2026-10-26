# CommonJS in JavaScript eval

JavaScript eval can import workspace-local `.cjs` modules through the retained
QuickJS runtime. It does not launch Node, install packages or grant Node host
APIs. This closes the explicit-CommonJS part of the package-loading gap, not
full Node compatibility.

```javascript
// analysis.cjs: module.exports = values => values.reduce((a, b) => a + b, 0);
const analysis = await import('./analysis.cjs');
analysis.default([10, 20, 12]);
```

A CommonJS body receives `exports`, `require`, `module`, `__filename` and
`__dirname`. `module.exports` is the ES module's **default export**; it may be
an object, function, primitive, null or undefined. There is no speculative
named-export analysis. Top-level variables stay inside the wrapper, top-level
return is valid, and `this` initially references `exports`.

Within the module, synchronous `require('./dependency.cjs')` and
`require('./data.json')` use explicit relative paths. Paths resolve from that
module, not the cell's working directory. `require.resolve()` returns the
admitted canonical filename without running its body. JSON is parsed as data,
not evaluated as an object literal, and does not require ES import attributes.
An installed package whose existing exports/main entry names a `.cjs` file can
also be imported through its package name.

## Identity, cycles and recovery

Canonical aliases share one CommonJS instance per module-graph generation.
The cache publishes partial exports before running the body, so cycles see
already-initialized properties. Replacing `module.exports` does not rebind a
previously obtained partial object. ES default imports and CommonJS requires
of a `.cjs` module share the same exported value and body execution.

A failed CommonJS require is removed from the CommonJS cache. It is not retried
automatically; an explicit later require may retry, while successfully loaded
dependencies remain cached. Side effects already performed are not rolled
back. QuickJS independently caches failed ES module imports: use the existing
`reloadModules()` operation before a later cell's first import to select a fresh
graph. Old functions keep their originating graph and require closures.

Reloading does not reset admission budgets. JSON required by CommonJS is cached
within this CommonJS graph; it is not promised to share object identity with a
separate ES JSON import.

## Ownership and limits

Resolution and reads use the existing native workspace boundary, admitted file
reader, current cell owner and absolute deadline. Every require, including a
cache hit, checks the current owner. A retained require function is not a
retained filesystem capability. Existing source, identity and resolution
limits apply: 2 MiB per source, 16 MiB accumulated source, 128 module identities
and declarations per realm, and 4,096 resolutions per cell. Failed source
loads count against the declaration allowance too. JSON nesting is limited to
64 containers, and synchronous require nesting to 64 modules. The cell's
native interrupt handler bounds JavaScript loops.

The cache is a realm-owned JavaScript closure, not Rust persistent roots or an
unbounded external registry. It disappears with realm reset/retirement. No
file is read and no module body runs during loader installation. A `.cjs` body
runs at module evaluation, not while an ES graph is still linking.

## Deliberate boundaries

This first surface requires explicit `.cjs`/`.json` relative dependencies.
Bare-package `require`, CommonJS `.js` package scopes, extension/directory
probing, `require.cache`, native addons and synchronous ESM require are not
provided. Neither are `process`, `Buffer`, filesystem/network built-ins, or a
cell-global require. Native tool access stays on the existing policy-checked
bridge. Dynamic ES import inside a CommonJS function has no supported file
origin; use the surrounding ES module/cell for asynchronous imports.

`%load` keeps its existing script semantics; it is not a CommonJS entrypoint.
Use `import('./file.cjs')` to request CommonJS semantics. Eval remains code
execution, not a security sandbox for arbitrary guest code.

## Tests

`node --test tests/eval_commonjs.test.cjs` runs the production wrapper/cache
with controlled native-loader receipts. It covers exports, cycles, state,
recovery, captured intrinsics and current-owner checks, but not the Rust file
resolver or QuickJS. Native regressions live in
`src/eval/js_kernel/modules/commonjs/tests.rs`, including real files, ES
linking, workspace policy, cancellation, reloads and package entrypoints.
The authoritative full quality command is `dsr quality --tool pi_agent_rust`.
