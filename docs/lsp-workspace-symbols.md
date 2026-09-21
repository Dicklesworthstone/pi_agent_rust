# Workspace-symbol navigation

Search project-wide names through the language server selected by an existing
anchor file. This is a native `symbols` workflow, not text grep:

```json
{"action":"symbols","file":"src/lib.rs","query":"Parser","resolve":true,"limit":20,"timeout":10}
```

The file's extension selects the configured server and its root markers select
the workspace. With `query` present, `file` is an anchor rather than a request
for that file's outline. The existing `symbol` anchor spelling also works:

```json
{"action":"symbols","symbol":"src/lib.rs","query":"Parser","resolve":true}
```

Supply one anchor, not both. The anchor must be a readable, regular nonsymlink
file of at most 2 MiB. The server is started lazily and the anchor synchronized
before search. Query text is passed verbatim; an empty string requests all
symbols the server chooses to report. Queries are at most 1024 UTF-8 bytes and
cannot contain NUL. No fuzzy matching, language identifier inference, or local
reordering is imposed on the server's result.

Without `query`, the existing document-outline operation is unchanged:

```json
{"action":"symbols","file":"src/lib.rs"}
```

`resolve` requires a workspace query. Other selectors, including `apply`, names,
ranges and positions, are not accepted by workspace search.

## Deferred locations

LSP 3.17 permits `WorkspaceSymbol` results with a URI but no range when the
client negotiates `workspace.symbol.resolveSupport`. Pi advertises support
for `location.range` and all standard numeric symbol kinds. See the
[protocol definition](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workspace_symbol).

Omit `resolve` or set it to false to receive inline results without additional
requests. With `resolve:true`, the server must advertise `resolveProvider`;
otherwise Pi returns `LSP_UNSUPPORTED`. Only retained results missing ranges
are sent to `workspaceSymbol/resolve`; symbols already carrying a range and
results omitted by the output limit incur no resolution request.

The exact original symbol, including opaque `data`, is sent back. Resolution
may add only `location.range`; changing its URI, name, kind, container, data,
tags or any other property is rejected. A result still missing its range is
an error, not a fabricated location. Server failures remain errors, including
failures after earlier symbols were resolved; no partial success is returned
in that case. No partial-result or work-done tokens are requested.

## Result boundaries

Tool details retain the existing `payload` envelope. Its `symbols` array
contains the retained LSP objects in original order, including their data,
container names, tags, and locations. Unknown numeric kinds and tags are
preserved without claiming a known meaning. Additional fields are:

- `count`, `total`, `truncated`, and `responseComplete`: retained count, server
  response count, whether any response entries were omitted, and its inverse.
- `locationsComplete` and `unresolvedIndices`: whether every retained symbol
  has a range, and the one-based indices still lacking one.
- `resolveRequested`, `resolveSupported`, and `resolvedCount`: requested mode,
  negotiated provider support, and the number resolved during this request.

An empty or null server response is valid absence. `responseComplete` describes
only the returned response, never index readiness, all project definitions,
all configured servers, or an atomic dependency snapshot. `locationsComplete`
applies only to retained symbols, including when `truncated` is true.

The default output count is 50, capped at 128. Admission is bounded to 4096
server entries and 2 MiB per response, 64 KiB per symbol, and 200 KiB of output
JSON. Every returned entry is validated, including ones beyond the count
limit. Output truncation keeps a prefix of whole objects rather than converting
JSON into a cut-off string. If resolution exceeds the remaining byte budget,
the request fails and the caller should narrow its limit or query.

## Freshness, authority, and external locations

The original caller's I/O authority, cancellation, live connection, exact anchor
text and synchronized document incarnation remain checked through startup,
search, resolution and delivery. Closing and reopening identical anchor bytes
invalidates the in-flight request. One timeout covers those phases after the
tool's serialized workflow lane is acquired. Queue waiting is cancellable but
outside that timeout; synchronous filesystem I/O cannot be preempted.

Returned locations are **metadata only**. Pi does not open their files, follow
external URIs, fetch documentation, execute embedded commands, or grant
`workspace/applyEdit` permission. Their ranges are checked for unsigned LSP
integer bounds and ordering, not against the contents of unopened targets.
Anchor freshness does not establish that every dependency or target file stayed
unchanged during the query.

Protocol tests use a real framed child peer through the public tool. The peer
is not a language analyzer; standalone Python peer checks do not execute the
Rust implementation. Native execution and language-server acceptance belong
to the repository's DSR quality lane.
