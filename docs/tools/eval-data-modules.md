# Workspace JSON modules in JavaScript eval

The retained JavaScript kernel can import local JSON through the same workspace
module loader as JavaScript and TypeScript. No package installation, network
fetch, new tool bridge, or separate data cache is involved.

```javascript
const rows = (await import('./data/rows.json', { with: { type: 'json' } })).default;
display({ total: rows.reduce((sum, row) => sum + row.amount, 0) });
```

Use `kernel: "js"` in the ordinary `eval` tool. Local `.mjs`, `.js`, `.mts` and
`.ts` modules may also use a static import:

```javascript
import settings from './settings.json' with { type: 'json' };
export const threshold = settings.threshold;
```

Static import declarations belong in a module file. An eval cell is still a
persistent script; use the awaited dynamic-import form in the first example.
Paths are explicit relative filesystem paths, rooted at the original workspace
for a cell and at the importing file for a transitive module. URLs, packages,
implicit extensions and paths outside the workspace remain unsupported.

## Data, not executable JavaScript

A `.json` file requires **exactly** `with: { type: 'json' }`. A missing, wrong or
extra attribute is rejected, including on subsequent imports of a cached file.
JavaScript and TypeScript modules still reject import attributes. Aliases cannot
change a data import into a code import or vice versa; in-workspace aliases to
the same JSON file share its canonical identity.

The sole export is `default`. Objects, arrays, strings, numbers, booleans and
null are supported. Parsing uses the engine's native JSON operation, not a
replaceable global `JSON.parse` or evaluated object literal. This preserves
JSON's own `__proto__` property, duplicate-key behavior, insertion order and
JavaScript number semantics, including negative zero. Large numbers are not
arbitrary-precision integers. No reviver, constructor, setter or user serializer
is invoked by loading the data. Existing objects/prototypes in the eval realm
remain ordinary JavaScript objects; this is not a sandbox for cell code.

Malformed JSON returns `EVAL_MODULE_JSON` without including the file's contents
in the parser diagnostic. It never executes a prefix of the file as code.
Successful data is available to the calling script and can contain secrets;
printing, returning or displaying it intentionally exposes that data to the
ordinary eval transcript. Error redaction is not general secret filtering.

## Lifetime and resource limits

The JSON default is mutable and cached once per canonical path in that retained
JavaScript realm. Importers share the same object, so one cell's mutation is
visible to another cell in that conversation. Editing the source file does not
reload an already imported module. Explicitly reset JavaScript to read a fresh
snapshot. Conversation switches retain their own existing eval namespaces;
JSON imports do not share objects across those realms or write mutations back
to disk. Host I/O permission and cancellation admission still apply when a
cached path is resolved again.

The existing loader bounds apply: regular UTF-8 workspace files of at most
2 MiB, at most 128 admitted paths/declaration attempts, and a 16 MiB cumulative
realm source budget shared with JS and TS. JSON adds a maximum of 64 nested
object/array containers. UTF-8 BOMs are accepted. Failed reads/parses consume
normal declaration/source budgets rather than providing an unbounded retry
path. Native parsing and linking run on the existing kernel thread under the
cell deadline; synchronous native work is checked cooperatively, not preempted
by a new worker. Filesystem race guarantees remain those of the existing bounded
source reader, not a transaction against external writers.

Tests cover native default exports, static/transitive imports, TypeScript,
cache identity, explicit reset, conversation ownership, typed displays, path
and attribute rejection, parser independence, malformed input and budgets.
The implementation environment lacks `dsr`; Rust compilation, formatting,
Clippy and these authored tests remain unexecuted. Acceptance requires:

```sh
dsr quality --tool pi_agent_rust
```
