# Call and type hierarchies

The `lsp` tool can explore the language server's call graph and type hierarchy.
These are read-only queries. They neither apply edits nor grant permission for
server-initiated `workspace/applyEdit` requests.

## Start from a symbol

```json
{"action":"incoming_calls","file":"src/service.rs","symbol":"handle_request","line":42}
```

Use `outgoing_calls` to find callees. The optional `line` is one-based; omit it
when the symbol is unique in the file, or use `symbol#N` to select an occurrence.
The position is computed from the same source snapshot synchronized to the server.

For types, use the corresponding actions:

```json
{"action":"supertypes","file":"src/model.ts","symbol":"Order"}
```

```json
{"action":"subtypes","file":"src/model.ts","symbol":"Order"}
```

Type relationships depend on the language and server: an interface or trait
relationship is not inferred locally from text. The server must advertise
`callHierarchyProvider` or `typeHierarchyProvider` as appropriate. Unsupported
capabilities are errors, not empty graphs. Dynamic registration is not supported.

## Follow a returned item

Each returned item and selected `source` includes a `hierarchyId`. Use that ID
instead of a file, line, or symbol to request another hop:

```json
{"action":"outgoing_calls","hierarchyId":"<ID from a call-hierarchy result>"}
```

```json
{"action":"subtypes","hierarchyId":"<ID from a type-hierarchy result>"}
```

Directions within one family can be mixed: a caller can be followed to its
callees, or a supertype to its subtypes. Call IDs and type IDs are deliberately
not interchangeable even though their protocol item shapes look alike.
The complete server item, including opaque `data` and unknown fields, is sent
back unchanged. Those internal fields are not exposed in the summary.

When preparation returns several roots, the result has `selectionRequired:true`
and lists choices with handles. No root is guessed. Select one handle in the next
request. Each request traverses only one hop; recursion and cycles do not trigger
automatic graph expansion.

## Interpret results

`count` is the number of displayed items; `total` is the number returned by the
server for this hop. `truncated` reports omitted items. `hierarchyKind` is `call`
or `type`. Ranges and selections use zero-based lines and UTF-16 character units.

Call entries also include `fromRanges`, `callSiteUri`, `callSiteCount`, and
`callSitesTruncated`. Incoming-call sites belong to each caller's URI. Outgoing
call sites belong to the selected source's URI, not the callee's. Type entries
have no call-site fields.

An explicit null/empty server response produces an empty result. Protocol errors,
malformed items, failed requests, cancellation, and expired handles do not become
successful empty results. A valid prefix with an invalid tail is rejected.

## Lifetime and bounds

Handles are local to a tool instance, the original synchronized source incarnation,
and the same live server. They expire after two minutes, on cache eviction, source
changes, document close/reopen, or server reload/retirement. Expired handles never
silently start a new server. Restart with `file` and `symbol`.

The original source is compared with its captured bytes before and after requests.
This is not a snapshot of every referenced file or proof that the server's whole
index is current. Returned resource URIs are labels; the tool does not read files,
fetch URLs, or start a new server based on them. There is no concurrent-filesystem
sandbox or whole-program completeness guarantee.

One response admits at most 1,024 items and 4 MiB of serialized data, with 64 KiB
per item. Display defaults to 100 items and is capped at 128 plus the selected
source, within a 200 KiB payload budget. Each displayed call has at most 64 complete
call-site ranges. The retained cache holds at most 256 handles, with a 64 MiB
budget for serialized item data and retained source bytes; this is not a bound on
all allocator overhead. Preparation and expansion share the requested timeout.
Synchronous filesystem and pipe operations are not a hard real-time guarantee.
