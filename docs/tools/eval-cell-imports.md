# Static imports in retained JavaScript cells

Native JavaScript eval accepts ordinary static-import declarations in a cell
or a `%load` JavaScript/TypeScript script. These declarations use the existing workspace
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

## Reusable typed setup files

`%load analysis/setup.ts` now accepts imports as well as TypeScript declarations:

```typescript
import rows from './rows.json' with { type: 'json' };
import { summarize } from './stats.ts';
import type { Report } from './types.ts';

const report: Report = summarize(rows);
display(report);
report
```

The existing SWC compiler erases types before the same retained-script import
preparation runs. Explicit type-only imports never load a file, and removing
them does not leave a synthetic empty export. Value imports remain even when
their bindings are used only in annotations, because their modules can have
side effects. The compiler does not fetch dependencies or perform type checking.

The setup file's real path remains the import origin. Its ordinary declarations
and imported bindings remain available to later cells. Exports and `import.meta`
are still rejected for `%load` scripts, including scripts that contain imports.
Use `import * as analysis from './analysis/report.mts'` to load an actual typed
module with exports. Declaration files, CommonJS TypeScript and TSX remain
unsupported. A malformed typed script is rejected before its body or imports
execute; errors in subsequently loaded dependencies retain the partial-effects
rules above. Reloading a setup file does not bypass retained lexical-name or
dependency-cache rules.

## Limits and validation

Preparation accepts at most 4 MiB of source/generated output, 128 static import
declarations and 512 imported names. The existing loader's shared module and
file budgets also apply; these are not extra module allowances. Parsing is
synchronous bounded work on the kernel thread, checked cooperatively by the
original cell deadline rather than made preemptible by a new worker.

Regression coverage exercises parsing, hoisting, all binding forms, attributes,
retained state, native module caching, failures and public EvalTool workflows.
Typed-script cases exercise code/data composition, type-only erasure, unused
value dependencies, persistent declarations and refusal before execution.
The implementation environment lacked `dsr`. These Rust tests are authored,
not executed; compilation, formatting, Clippy and runtime acceptance require:

```sh
dsr quality --tool pi_agent_rust
```
