# Source-checked document outlines

The native `lsp` tool's `symbols` action with `file` and **without `query`**
inspects definitions in that file. This uses the language server, including
streamed document-symbol responses, rather than searching text. `query` still
selects the existing workspace-symbol search; it is not a document-name filter.

```json
{"action":"symbols","file":"src/service.rs","limit":100}
```

The response keeps an object in `details.payload`, even when a large outline
must be shortened. `symbols` contains complete rows in the server's preorder,
with `index`, `parentIndex`, `depth`, `name`, `kind`, `range`, `selectionRange`,
and supplied detail/tag/deprecation metadata. An index refers to this complete
outline, not an immutable handle that can be reused after source changes.

## Find definitions instead of comments and call sites

```json
{"action":"symbols","file":"src/service.rs","symbol":"handle"}
{"action":"symbols","file":"src/service.rs","symbol":"handle#2"}
{"action":"symbols","file":"src/service.rs","symbol":"handle#1","line":120}
```

`symbol` is a case-sensitive literal substring of a **definition's name**.
There is no regex evaluation. As with Pi's other symbol selectors, a positive
`#N` suffix selects the Nth match in server order. For document outlines this
counts definitions, not occurrences in comments or call sites. An unavailable
ordinal is an error, not permission to pick a different definition. An ordinary
unnumbered name search with no matches returns an empty structured result.

`line` is one-based and selects every definition whose full half-open range
intersects that line. This can include both a method and its enclosing class.
An end position at column zero does not include that next line; a zero-width
symbol marker belongs to its own line. The line must exist in the source, and
is validated before language-server startup. Name and line filters intersect
**before** numbered selection and display limits.

Each returned hierarchical row includes root-first `ancestors` with the
containing definitions' original indices, names, kinds and ranges. Filtering
out a class therefore does not make two identically named methods ambiguous
by stripping their context. Flat `SymbolInformation` results retain
`containerName` and `location`; they have no invented parents or identifier
selection ranges, and `ancestors` is empty. A flat location must refer to the
requested file. Returned URIs are not opened.

Use a hierarchical `selectionRange.start` with a later navigation or rename
request, or `range` to select a definition for code actions. The outline is
read-only information, **not a refactor approval handle**. Those later actions
still perform their own source and permission checks.

## Counts and completeness

`total` counts every validated symbol, including nested and filtered-out
symbols. `matched` counts matches after name/line filtering. `selectedCount`
applies the optional ordinal to those matches. `count` is the number of rows
actually displayed. `truncated` means display limits omitted selected rows;
intentional name/line filtering is not truncation. `responseComplete: true`
means the complete protocol response was received and validated, not that
all selected symbols fit the output or that the server found every possible
definition in the project.

The default display limit is 100, capped at 1000 and the 200 KiB structured
output budget. Rows and their ancestor context are not cut into partial JSON.
Source and complete response limits are 2 MiB each, with at most 8192 symbols
across all hierarchy levels and at most 32 levels. Symbol names are limited to
4096 bytes, optional detail to 16 KiB, and the input name selector to 1024 bytes.
These bounds include symbols that filters or display limits will omit.

All source positions are exact zero-based UTF-16 boundaries. The validator
handles CR, LF and CRLF and rejects split surrogate pairs, nonexistent lines,
reversed ranges and selections outside their symbol's range. It checks all
distinct endpoints in one source traversal, rather than rereading the source
for every symbol.

The operation reuses the existing semantic source checks and owned partial
result collector. Source changes, malformed members, mixed response forms,
lost fragments, cancellation, failure after progress, and ambiguous streamed
terminal responses are errors rather than partial success. A single remaining
budget covers startup, synchronization, requests and result validation after
the tool's serialized workflow admission. Dropping a pending call retires its
collection and releases that workflow. Synchronous filesystem calls are not
preemptible; this is not an atomic snapshot against external writers.

No editor changes, server commands, dependency changes or new public tool are
introduced. Server support for `documentSymbolProvider` is required. Regression
coverage is in `src/lsp/semantic/outline/tests.rs`, including framed-subprocess
calls through `LspTool`. The quality authority remains
`dsr quality --tool pi_agent_rust`; source inspection and Python fixture checks
are not Rust integration or platform qualification.
