# Reusable JavaScript eval files

Run an inspected, workspace-local JavaScript script in the retained QuickJS
kernel using the existing `eval` tool:

```json
{"kernel":"js","code":"%load 'analysis/setup data.js'"}
```

A loaded script executes exactly as an inline cell. Ordinary script-level
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

This is file-backed **script** execution, not a Bun/Node/TypeScript module loader.
Relative imports, package resolution, `%bun`, and source-mapped stack labels are
not implemented by this feature. Python's existing `%load` path is unchanged.
