# Workspace JavaScript modules in eval

The retained QuickJS kernel supports local ES modules through `import()`:

```json
{"kernel":"js","code":"globalThis.math = await import('./analysis/math.mjs'); display(math.answer)"}
```

Imported `.js` and `.mjs` files use ES module syntax. They may use static
imports, re-exports, live bindings, cycles and top-level await. They also use
the same timers, read-only tool bridge, rich displays and console capture as
ordinary cells. Importing a module does not create another runtime or grant
new tool permissions. For example, `analysis/math.mjs` can contain:

```javascript
import {base} from './constants.js';
await sleep(1);
export const answer = base + 2;
```

Inline cells resolve imports from the primary workspace. Existing `%load`
JavaScript scripts now carry their canonical source filename into QuickJS,
so their imports resolve beside the script. A function defined by that script
keeps its importer origin when invoked in a later cell. Inline cells do not
inherit the last loaded script's directory. Imported modules expose a correctly
encoded canonical file URL in `import.meta.url`.

`%load setup.js` remains a script cell with persistent script bindings; it is
not converted into a module and static import declarations in that entry cell
are not supported. Use `await import('./setup.mjs')` for a module entry and
`globalThis` or an exported namespace for values to retain. Local module names
do not become globals automatically.

## Resolution, caching and reset

Use explicit relative specifiers starting with `./` or `../`, including the
`.js` or `.mjs` extension. There is no package search, extension guessing,
CommonJS `require`, Node/Bun built-in, TypeScript transpilation, native-library
loader, remote URL, virtual scheme, query string, fragment or import attribute.
Use forward slashes in import specifiers on all platforms. A parent traversal
is permitted only when the resolved file remains inside the primary workspace.
Symlink aliases within the workspace share their canonical module identity;
symlinks escaping it are refused. Source is UTF-8; an initial BOM is removed.

QuickJS owns module linking and caching. Successfully evaluated modules are
not rerun by later imports of the same canonical file, and their live state
survives later cells in the same retained kernel. Editing the file or reloading
a script does not invalidate the module cache. `reset: true` clears the selected
JavaScript namespace and its modules. Other instances and retained session
namespaces are unaffected. The resolver still requires a valid workspace-local
target when importing a cached module; deleting or moving it can make a later
import fail. Parse and execution failure behavior follows the native engine.

## Lifetimes and bounds

Await module imports and their asynchronous work. There is no detached module
execution between cells. Existing cell cancellation, deadlines and realm-reset
rules apply, including top-level await in imported modules. Code already
executed is not rolled back after a failure. Module file access checks the
current invocation's I/O authority and cancellation, not the authority of an
earlier cell that created the kernel.

File reads reuse the bounded `%load` reader: regular files only, at most 2 MiB
each, with no-follow/nonblocking leaf opening on Unix and observed metadata
change detection. A realm permits 128 admitted module names, 128 compilation
attempts and 16 MiB of compiled source, with 4096 resolutions per cell and
4096-byte paths. Failed attempts consume limits too; reset clears the realm's
budgets. These are host input bounds, not a sandbox or a general JavaScript
heap limit. Synchronous filesystem calls can outlast a cooperative deadline;
identity checks do not provide an atomic snapshot against a hostile writer.

Regression coverage is in `src/eval/js_kernel/modules/tests.rs` and the public
`tests/eval_modules.rs`. Run the repository's `dsr quality --tool pi_agent_rust`
entry point for Rust validation. Source inspection alone is not a passing test
or QuickJS interoperability result.
