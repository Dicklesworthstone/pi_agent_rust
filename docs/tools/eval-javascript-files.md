# Reusable JavaScript and TypeScript eval files

Run an inspected, workspace-local JavaScript script in the retained QuickJS
kernel using the existing `eval` tool:

```json
{"kernel":"js","code":"%load 'analysis/setup data.js'"}
```

A loaded JavaScript script executes exactly as an inline cell. Ordinary script-level
bindings persist; top-level-await cells use `globalThis` for persistent values.
The existing timers and read-only `tool.call` bridge work inside the script.
A subsequent `%load` rereads the current file; merely editing it does not reload
it. Re-declaring an existing `const`/`let` has normal JavaScript semantics; use
`reset:true` deliberately when a fresh realm is required.

The command must occupy the entire cell and take one path. Matching single or
double quotes permit spaces. Backslashes, dollar signs, and other characters
are literal, not shell syntax; quotes cannot be escaped inside a quoted path.
Use forward slashes on Windows. Paths are resolved against the tool's primary
workspace. URL schemes (including `local://`) and virtual documents are refused.
Download and inspect remote source separately before saving it locally.

Admission requires a regular file under the canonical workspace root, no larger
than 2 MiB and encoded in UTF-8. A leading UTF-8 BOM is removed. Reads are capped
and file identity/metadata changes observed while reading are rejected. These
checks are not an atomic filesystem snapshot against a hostile parent-directory
writer, and eval itself is not a sandbox. Additional workspace grants do not
automatically authorize script loading.

File preparation occurs after exclusive cell admission and before a requested
reset. A malformed command, missing file, invalid encoding, or rejected path
therefore leaves retained state and the cell counter untouched, even with
`reset:true`. Dropping a queued/preparing call cannot execute the script. File
reading is offloaded; the selected runtime's cell timeout begins on execution.
The read worker can finish its bounded read after the awaiting call is dropped.

Results include `details.sourcePath` and `details.sourceCommand`, not a second
copy of the source. Normal JavaScript exceptions include a filename notice.
The engine's stack traces still use eval labels. Runtime errors preserve already
completed mutations; syntax errors do not execute the script prefix. Neither
case is retried. Existing timeout/crash cleanup rules still apply.

This is file-backed **script** execution. Relative dynamic imports use the native
eval module graph described in `docs/eval-modules.md`, not the extension loader.
Node/Bun package resolution, `%bun`, and source-mapped stack labels are not
implemented by this feature. Python's existing `%load` path is unchanged.

## Typed scripts

The same JavaScript kernel can load a self-contained `.ts` file:

```json
{"kernel":"js","code":"%load 'analysis/setup data.ts'"}
```

```typescript
interface Observation { value: number }
enum Score { Base = 40, Next }
class ObservationBox {
    constructor(public value: number) {}
}
function identity<T>(value: T): T { return value; }
const observation: Observation = new ObservationBox(identity(Score.Next + 1));
observation.value
```

The project's existing SWC compiler parses the inspected bytes, strips types,
lowers TypeScript runtime constructs, resolves generated identifiers, and emits
a JavaScript script for the existing retained QuickJS evaluator. Interfaces,
type aliases, annotations, generics, assertions, `satisfies`, enums and parameter
properties use that compiler pipeline, not textual replacement. This is **not
type checking**: missing types or incompatible assignments are not diagnosed by
a TypeScript type checker. No `tsconfig.json`, compiler plugin, shell command,
package manager, or external compiler process is loaded by this preparation.

Ordinary top-level bindings remain available in later cells. Top-level await,
timers, the read-only tool bridge, console updates/archives and rich displays
reuse the same eval machinery. As with JavaScript async cells, assign retained
async results to `globalThis`; declarations inside async eval are not persistent.
Changing the file does not reload it. Each explicit `%load` recompiles the current
file, and an explicit reset is still needed to redeclare existing `const`/`let`
bindings. There is no transpiled-script cache or second runtime namespace.
Imported modules retain the native cache described below.

The canonical filename's `.ts` extension selects this preparation (case
insensitive). Inline `kernel:"js"` code and ordinary JavaScript files are not
implicitly parsed as TypeScript. `.d.ts`, `.tsx` and `.cts` files are refused
rather than inventing declaration, JSX or CommonJS execution rules. `.mts` files
must be loaded as modules with `await import('./file.mts')`.
Top-level imports and exports, **including explicit type-only declarations**,
are refused in a `%load` entry before stripping; use a genuine module entry
instead of silently changing binding scope. Dynamic `import()` works in typed
scripts and functions defined by them retain the original script's importer
directory when called later. The native resolver admits explicit workspace-local
`.js`, `.mjs`, `.ts` and `.mts` imports under its existing authority and limits.
Actual imported modules retain static imports/exports and `import.meta.url`;
unused value imports are preserved for side effects, while explicit type-only
imports are erased without loading the referenced module. Their cache and live
bindings are owned by QuickJS, and editing a file does not invalidate the cache.
`import.meta` remains unavailable in script entries. TypeScript CommonJS import
assignments and decorators are refused. This is transpilation, not Node/Bun
package compatibility or a JSX/decorator transform.

Compilation runs on the existing source-read worker after exclusive admission
and before reset or kernel execution. Both fatal and recovered parser errors
refuse the entire source with `EVAL_LOAD_TYPESCRIPT` and its filename/line/column.
Rejected syntax or unsupported loader constructs cannot run an earlier statement,
clear the old realm, or increment its cell sequence, even with `reset:true`.
Runtime failures still preserve completed mutations and are not retried.
Runtime stack labels are not source-mapped back to TypeScript; only compiler
diagnostics carry original source positions.

The existing 2 MiB source limit remains, and the emitter refuses JavaScript above
4 MiB while generating it, preserving the native module compiler's existing
limit. These bounds are not a global parser/guest memory or
CPU sandbox. Compilation is preparation, excluded from the active cell timeout;
owner cancellation prevents subsequent execution but cannot preempt a running
synchronous compiler step. Compilation of an imported typed module is instead
part of active cell execution and counts against its deadline. The native eval
module resolver remains the authority for those imports; no extension realm,
network API or additional tool permission is inherited by these cells.

Compiler and public-tool regression coverage lives under
`src/eval/lifecycle/source/typescript.rs` and `tests/eval_file_js.rs`. Use
`dsr quality --tool pi_agent_rust` for authoritative validation.
