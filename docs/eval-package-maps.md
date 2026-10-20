# Package export maps in eval

Workspace-installed ESM packages can expose wildcard subpaths and ordered
alternative targets through their existing `package.json` exports. Inline
imports, loaded scripts and transitive JavaScript/TypeScript modules all use
this same resolver. No installation, network lookup or Node/Bun built-ins are
added.

```json
{
  "type": "module",
  "exports": {
    ".": ["./index.mjs", "./fallback.mjs"],
    "./features/*": {"import": "./src/features/*.mts"},
    "./features/private/*": null,
    "./data/*": "./data/*.json"
  }
}
```

```javascript
const feature = await import('installed-package/features/summary');
const data = await import('installed-package/data/example', {with: {type: 'json'}});
display([feature, data.default]);
```

Exact entries win over patterns. Otherwise the longest prefix before `*` wins;
a longer full key breaks a prefix tie. Each pattern key has exactly one star,
and its nonempty capture may contain multiple path segments. All stars in the
selected target are replaced literally. There is no glob scan or extension
search. A specific null entry blocks broader exports. Invalid/mixed map keys
are refused rather than interpreted opportunistically.

Conditions retain author order, matching only `import` and `default`. Arrays
select the first syntactically valid target, skipping invalid targets,
unmatched conditions and null elements. Empty arrays block the entry. Invalid
configuration and budget failures are not skipped. Once a target is selected,
a missing file, unsupported format, permission failure or compilation error
fails the import: none triggers fallback to another array element or pattern.
In particular the example's missing `index.mjs` does not try `fallback.mjs`.

Targets must remain explicit `./` paths. Templates and expanded paths reject
traversal, URL/query/fragment syntax, encoded separators, `node_modules`, and
ambiguous path segments. Expanded strings are bounded before allocation.
Canonical file/package/workspace containment, supported format and explicit
data-attribute checks remain authoritative after selection. These are existing
file-admission guarantees, not a sandbox for arbitrary eval code.

Maps share the existing generation-scoped metadata cache. `reloadModules()`
loads edited maps for new graphs, while old namespaces and functions keep their
old graph's snapshots. Reload does not reclaim realm limits. Existing 64 KiB
manifest, depth/node, 128 metadata-lookup and 2 MiB metadata allowances apply;
module identity, compilation, source and path bounds are unchanged.

Package-private `#imports`, self-reference and CommonJS are not implemented by
this export-map change. Regression tests in `packages/maps.rs` cover map
selection and the actual public EvalTool, including typed/data imports, reloads,
sealed entries, missing files and canonical symlink containment. Rust acceptance
requires `dsr quality --tool pi_agent_rust`; reference probes in another runtime
are not evidence that the Rust implementation compiles or passes its tests.
