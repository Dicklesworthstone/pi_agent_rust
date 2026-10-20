# Workspace JavaScript and TypeScript modules in eval

The retained QuickJS kernel supports local ES modules through `import()`:

```json
{"kernel":"js","code":"globalThis.math = await import('./analysis/math.mts'); display(math.answer)"}
```

Imported `.js`, `.mjs`, `.ts`, and `.mts` files use ES module syntax. TypeScript
source is parsed and stripped with the SWC stack already linked into Pi, then the
resulting JavaScript is handed to the same QuickJS linker and cache as ordinary
JavaScript modules. There is no Bun/Node subprocess and no second module runtime.

Modules may use static imports, re-exports, live bindings, cycles and top-level
await. Type-only imports/exports and type annotations are removed; runtime
imports remain native ES-module edges. TypeScript enums and other syntax lowered
by SWC execute in QuickJS. TSX/JSX is deliberately not enabled.

All modules use the same timers, invocation-owned tool bridge, rich displays and console
capture as ordinary cells. Importing a module does not grant new tool
permissions. For example, `analysis/math.mts` can contain:

```typescript
import type {Shape} from './shape.ts';
import {base} from './constants.ts';

await sleep(1);
const value: Shape = {answer: base + 2};
export const answer: number = value.answer;
```

Inline cells resolve imports from the primary workspace. Existing `%load`
JavaScript scripts carry their canonical source filename into QuickJS, so their
imports resolve beside the script. A function defined by that script keeps its
importer origin when invoked in a later cell. Inline cells do not inherit the
last loaded script's directory. Imported modules expose a correctly encoded
canonical file URL in `import.meta.url`.

`%load setup.js` and `%load setup.ts` are script cells with persistent bindings;
typed scripts use the same SWC compiler as imported modules. Each is
not converted into a module and static import declarations in that entry cell
are not supported. Use `await import('./setup.mts')` or `await import('./setup.ts')`
for a module entry and `globalThis` or an exported namespace for values to retain. Local
module names do not become globals automatically.

## Resolution, transpilation, caching and reset

Use explicit relative specifiers starting with `./` or `../`, including one
of the supported extensions. There is no package search, extension guessing,
CommonJS `require`, Node/Bun built-in, TSX/JSX transform, declaration-file
runtime, native-library loader, remote URL, virtual scheme, query string,
fragment, or import attribute. Use forward slashes in import specifiers on all
platforms. A parent traversal is permitted only when the resolved file remains
inside the primary workspace.

TypeScript lowering is fail-closed. Recovered parser errors are treated as hard
errors, so malformed TypeScript cannot execute a valid-looking prefix. Generated
JavaScript is limited to 4 MiB per module during generation, before QuickJS
compilation, rather than allocating an oversized output and rejecting it later. Source maps
are not emitted; engine stack traces identify the canonical module path but do
not claim TypeScript source-position remapping.

The shared transform preserves unused value imports for their runtime side
effects and removes only explicit type-only imports. Erased type references do
not resolve or read their target files. Native class fields and mutable enum
lowering preserve later runtime mutations instead of substituting old enum
constants. This is not type checking: no tsconfig, type checker, compiler plugin,
or package resolver is loaded. Declaration files, decorators and CommonJS import
or export assignments are refused explicitly rather than silently omitted.

Typed `%load` scripts compile on the source worker before a requested reset;
invalid syntax cannot discard existing variables or increment the cell sequence.
Imported modules compile during the active cell under the existing ownership
and deadline checks. A failed module cannot execute its own prefix, but earlier
side effects in its importing cell are not rolled back. See
`docs/tools/eval-javascript-files.md` for script syntax and reset behavior.

Symlink aliases within the workspace share their canonical module identity;
symlinks escaping it are refused. Source is UTF-8; an initial BOM is removed.
JavaScript and TypeScript files that canonicalize to the same path use one native
QuickJS module identity.

QuickJS owns module linking and caching. Successfully evaluated modules are not
rerun by later imports of the same canonical file, and their live state survives
later cells in the same retained kernel. Editing the file does not invalidate
that cache. `reset: true` clears the selected JavaScript namespace and its
modules, including transpiled TypeScript. Other tool instances and retained
session namespaces are unaffected. The resolver still requires a valid
workspace-local target when importing a cached module; deleting or moving it can
make a later import fail. Parse and execution failure behavior follows the
native engine.

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
attempts and a 16 MiB source allowance, with 4096
resolutions per cell and 4096-byte paths. Failed attempts consume limits too;
reset clears the realm's budgets. TypeScript's generated-output limit is
independent from the aggregate realm limit. Each module is charged the larger
of its original and generated sizes: erased types and expanded enums cannot
bypass that allowance. These are host input/output bounds, not a
sandbox or a general JavaScript heap limit.

Synchronous filesystem and TypeScript transformation work can outlast a
cooperative deadline between checks; authority and deadline are checked before
and after those stages. Identity checks do not provide an atomic snapshot
against a hostile writer.

Regression coverage is in `src/eval/js_kernel/modules/tests.rs`,
`src/eval/js_kernel/modules/typescript.rs`, `tests/eval_modules.rs`, and
`tests/eval_typescript.rs`. Shared compiler tests live in
`src/eval/lifecycle/source/typescript.rs`, with typed script/graph workflows in
`tests/eval_file_js.rs` and additional source-budget tests beside the module adapter.
Run the repository's
`dsr quality --tool pi_agent_rust` entry point for Rust validation. Source
inspection alone is not a passing test, compile result, or live QuickJS/SWC
interoperability result.
