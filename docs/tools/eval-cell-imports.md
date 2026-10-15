# Static imports in retained JavaScript cells

Native JavaScript eval accepts ordinary static-import declarations in a cell
or a `%load` JavaScript script. These declarations use the existing workspace
module loader; no package manager, additional runtime or network loader is added.

```json
{
  "kernel": "js",
  "code": "import rows from './data/rows.json' with {type:'json'}; import * as stats from './analysis/stats.mjs'; display(stats.summarize(rows));"
}
```

Default imports, named imports with aliases, namespace imports, side-effect-only
imports and combined default/named or default/namespace declarations are accepted.
JSON and text imports use the same explicit attributes as dynamic imports:

```javascript
import template from './templates/report.md' with { type: 'text' };
import { summarize as analyze } from './analysis/stats.mjs';
import './analysis/register.mjs';
```

Paths remain explicit relative workspace paths. In cells they resolve from the
kernel workspace; in `%load analysis/setup.js` they resolve from that file's
directory, including transitive imports. Local JS/TS module files retain their
normal native ESM linker. Packages, remote URLs and workspace escapes are still
refused by the existing loader, which retains its caller-I/O checks, canonical
cache, file limits and cancellation policy. No import is a new grant of authority.

## Retained-script semantics

This is convenience syntax for a persistent script, not conversion of the cell
into a separate ES module. SWC parses the entire input without executing it.
Imports are hoisted in declaration order and awaited before non-directive body
statements. There is no wrapper around the user's body: its declarations and
last-expression result keep ordinary eval behavior. The directive prologue,
shebang and body newlines are preserved. No generated helper names are retained
in the global namespace.

**Named and default imports are immutable binding snapshots.** Importing
`{count}` captures that export's value after dependencies have loaded; it does
not create an ES module live binding. `import * as counter` retains the actual
module namespace, so `counter.count` observes later changes to exported bindings.
Imported objects remain shared objects and can be mutated. Use namespace imports
for changing state, or put code requiring full ESM semantics in a module file.

Imported bindings survive subsequent cells in that conversation. Like other
`const` declarations, redeclaring the same name can fail; reuse the binding,
choose a new alias, or explicitly reset JavaScript. File edits do not invalidate
the retained module cache. An explicit reset clears bindings and the cache;
switching conversations selects its separate existing eval realm.

Exports and `import.meta` remain module-file syntax, not cell syntax. Source and
deferred imports are not implemented. Inline TypeScript is not inferred. Named
imports referring to absent exports fail explicitly rather than becoming
`undefined`. Values and export names are not included in that diagnostic.

Imports use native dynamic loading internally. Consequently, an earlier
dependency may already have run when a later dependency fails or lacks an
export. Its effects are not rolled back. Import declarations can remain
uninitialized after failure, just like a failed awaited `const` initializer.
Inspect the result and reset deliberately when needed; there is no automatic
replay of the cell, a dependency, or a runtime-thrown SyntaxError.

## Limits and validation

Preparation accepts at most 4 MiB of source/generated output, 128 static import
declarations and 512 imported names. The existing loader's shared module and
file budgets also apply; these are not extra module allowances. Parsing is
synchronous bounded work on the kernel thread, checked cooperatively by the
original cell deadline rather than made preemptible by a new worker.

Regression coverage exercises parsing, hoisting, all binding forms, attributes,
retained state, native module caching, failures and public EvalTool workflows.
The implementation environment lacked `dsr`. These Rust tests are authored,
not executed; compilation, formatting, Clippy and runtime acceptance require:

```sh
dsr quality --tool pi_agent_rust
```
