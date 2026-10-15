# Workspace data modules in JavaScript eval

The retained JavaScript kernel can import local JSON and text assets through the same
workspace module loader as JavaScript and TypeScript. No package installation, network
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

## Text assets for analysis and templates

Use the explicit native text-loader attribute for a supported text asset:

```javascript
const template = (await import('./templates/report.md', { with: { type: 'text' } })).default;
const csv = (await import('./data/results.csv', { with: { type: 'text' } })).default;
```

Static imports in a local JS/TS module use `with { type: 'text' }` too. The
supported suffixes are `.txt`, `.md`, `.csv`, `.tsv`, `.svg`, `.html`, `.css`,
`.xml`, `.yaml`, `.yml`, `.toml`, `.jsonl` and `.ndjson`. These all export one
**string**, not parsed tables, markup, configuration objects or JSON records.
The calling analysis module chooses how to parse or transform that string.
Empty files export the empty string. HTML/SVG/CSS is not rendered, styles are
not installed, and script-looking contents are never executed by importing it.

Text is strictly UTF-8 and preserves its original BOM, CRLF, embedded NUL,
quotes, backticks and Unicode characters. The loader creates a native string;
it does not interpolate a generated JavaScript literal or call a guest's
`String` constructor. An invalid UTF-8 source is refused without lossy decoding.
The new verbatim mode shares the existing bounded file reader and all of its
path, opened-file identity and special-file checks. Script/JSON reads still
strip a leading encoding BOM; text imports do not alter that existing behavior.

Text imports require exactly `type: 'text'`, including on cached paths. JSON
still requires `type: 'json'`; a JSON filename cannot be imported as text or
vice versa. Aliases may share canonical text files across the supported text
suffixes, but cannot cross text/JSON/executable-code boundaries. This is a
deliberate native loader extension, not universal browser/Node module syntax.

## JSON data, not executable JavaScript

A `.json` file requires **exactly** `with: { type: 'json' }`. A missing, wrong or
extra attribute is rejected, including on subsequent imports of a cached file.
JavaScript and TypeScript modules still reject import attributes. Aliases cannot
change the data type or turn a data import into code; in-workspace aliases to
the same JSON file share its canonical identity.

The sole JSON export is `default`. Objects, arrays, strings, numbers, booleans and
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

Data modules are cached once per canonical path in the retained JavaScript
realm. JSON importers share the same mutable object, so one cell's mutation is
visible to another cell in that conversation. Editing the source file does not
reload an already imported module. Explicitly reset JavaScript to read a fresh
snapshot. Text modules likewise keep their original imported string until reset,
even after the source changes. Conversation switches retain their own eval namespaces;
data imports do not share objects across those realms or write mutations back
to disk. Host I/O permission and cancellation admission still apply when a
cached path is resolved again.

The existing loader bounds apply to JSON and text: regular UTF-8 workspace files of at most
2 MiB, at most 128 admitted paths/declaration attempts, and a 16 MiB cumulative
realm source budget shared with JS and TS. JSON adds a maximum of 64 nested
object/array containers. UTF-8 BOMs are accepted. Failed reads/parses consume
normal declaration/source budgets rather than providing an unbounded retry
path. Native parsing and linking run on the existing kernel thread under the
cell deadline; synchronous native work is checked cooperatively, not preempted
by a new worker. Filesystem race guarantees remain those of the existing bounded
source reader, not a transaction against external writers.

Tests cover native JSON/string default exports, static/transitive imports, TypeScript,
cache identity, explicit reset, conversation ownership, typed displays, path
and attribute rejection, parser independence, malformed input and budgets. Text
cases also cover exact encoding preservation, every admitted suffix, cached-path
permissions, special files and mixed JSON/text TypeScript analysis through the
public tool. These are authored tests, not successful execution evidence.
The implementation environment lacks `dsr`; Rust compilation, formatting,
Clippy and these authored tests remain unexecuted. Acceptance requires:

```sh
dsr quality --tool pi_agent_rust
```
