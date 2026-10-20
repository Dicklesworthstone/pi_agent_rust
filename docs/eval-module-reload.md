# Reload modules without resetting eval

After editing a local JavaScript or TypeScript dependency, call `reloadModules()`
before importing it again:

```json
{"kernel":"js","code":"reloadModules(); globalThis.analysis = await import('./analysis/main.mts'); display(analysis.answer)"}
```

The synchronous helper starts a new native module-graph generation and returns
its number. It does not reset the Python or JavaScript kernel, erase variables,
restart cell counters, run a package manager, or execute modules by itself.
It works in both inline cells and `%load` scripts through the existing QuickJS
loader. Imports in the fresh graph use current admitted source, including
transitive JavaScript, TypeScript, JSON and text dependencies. Data imports still
require the correct explicit import attributes.

Call the helper at most once per cell and **before that cell's first import
attempt**, even if the attempted import failed. A late or repeated call raises
`EVAL_MODULE_RELOAD_ORDER` without another generation change. In particular a
module cannot rotate the graph while it is linking or evaluating. Invoking the
helper in a separate cell before your next import is also valid.

## Existing state and references

Previously retained namespaces and functions remain valid. They continue to use
their original module identities and live bindings. Importing from an inline
cell or a file-backed script selects the current generation. Imports originating
inside an older module select its old generation, so an old function cannot
silently rebind to the new copy of an already imported dependency. Module cycles
and linking remain the native engine's responsibility.

Assign the fresh namespace to a variable deliberately. The helper cannot rewrite
existing object references or lexical `const` bindings. A syntax error in a fresh
module leaves the previous namespace usable; fix the file and request another
reload to retry. Runtime side effects already performed before an error are not
rolled back, and a successful reload request is not rolled back by a later error
in the same cell. Unloaded lazy dependencies read files when first imported;
this is not an atomic snapshot of the workspace.

The physical importer directory remains unchanged. `%load` scripts retain their
canonical source origin, even if that file was previously imported as a module.
`import.meta.url` remains the canonical file URL, not the internal cache key.
Engine diagnostics can include a `pi-eval-module:<generation>:` identity prefix.

Ordinary imports remain cached; editing alone does not trigger reevaluation.
`reset: true` still discards the whole selected JavaScript namespace and clears
all module generations. Other tool instances and retained session namespaces
are unaffected by a reload.

## Limits and boundaries

Every admitted version counts toward the existing 128 module identities,
128 declaration attempts and 16 MiB source budget for the whole realm. Reload
never resets those counters. Exhaustion requires an explicit JavaScript reset;
QuickJS cannot safely unload modules that existing functions may still reference.
The helper checks the current cell's I/O authority, deadline and cancellation,
not the permissions of the cell that originally created it. It grants no extra
filesystem roots, package paths or host tools.

Package entry selection still follows the retained package-metadata catalog in
this implementation. Reload refreshes code and data selected by that catalog,
not changed `package.json` exports or previously missing manifest entries.

Tests exercise the actual public EvalTool, native linking, transitive typed/data
imports, old references, script origins, invalid attributes, reset and admission
limits. Run `dsr quality --tool pi_agent_rust` for repository acceptance.
This implementation environment has no DSR, so Rust compilation, tests, rustfmt
and Clippy have not run. This adds a native explicit reload workflow, not OMP's
automatic per-cell module invalidation or full Bun compatibility.
