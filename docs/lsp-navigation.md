# Source-bound navigation

`definition`, `references`, `type_definition`, `implementation`, and `hover`
accept an exact cursor in the source file:

```json
{"action":"definition","file":"src/lib.rs","position":{"line":8,"character":17},"limit":100,"timeout":10}
```

Positions use zero-based lines and UTF-16 code units, not UTF-8 bytes. Surrogate
interiors, out-of-bounds positions, and mixed cursor/symbol selectors are
rejected before server startup. The existing `symbol` selector, optional
one-based `line`, and `symbol#N` disambiguation remain available instead of
`position`. Navigation is read-only: `apply`, action/refactor handles, ranges,
format options and other unrelated selectors are rejected.

## Locations that can be used without guessing

Each location retains its exact `uri` and zero-based `range`, alongside the
existing display `file` and one-based `line` / `character` fields. A negotiated
location link additionally retains `targetUri`, the full `targetRange`, the
more precise `targetSelectionRange`, and optional `originSelectionRange`.
The normalized `range` and one-based coordinates point at the target selection,
not the start of its enclosing definition body.

The client advertises link support for definition, type definition and
implementation. References retains its standard Location-array contract and
includes declarations as before. Results preserve server order and duplicates.
No location URI is opened or fetched. Generated documents, dependency locations
outside the workspace and non-file URIs remain metadata; a later read needs its
own admission and authority. Target ranges are checked structurally, not against
unread target text. Origin ranges are checked against the retained source text.

A response must be null or a valid standard location form. Missing ranges,
reversed ranges, invalid unsigned integers, nonabsolute URIs, mixed location/link
arrays and target selections outside their full target range fail with
`LSP_NAVIGATION_MALFORMED`. Even entries omitted by the output limit are validated;
an invalid tail cannot disappear behind a successful partial result.

`total` counts the locations in this server response; `count` counts returned
entries. `truncated` and `responseComplete` describe output coverage only, not
whether the server indexed every reference in the project. The default limit is
100, capped at 1,000. A 200 KiB serialized output budget may retain fewer entries.
Only whole location records are retained. Responses over 2 MiB or 4,096 server
locations fail explicitly rather than being misreported as empty.

## Hover information

Hover retains the original `contents` representation, including markdown versus
plain text and language-tagged snippets, plus its optional source `range`. The
existing `hover` summary remains available. `returnedNull` distinguishes no
report from an explicitly empty string or array. Malformed array members, missing
content, unsupported markup forms and nonexact source ranges are errors, not
silently discarded text. An oversized complete hover result fails rather than
truncating a JSON object or code sample.

## Lifetime and boundaries

These workflows reuse semantic inspection's 2 MiB source admission, current
caller's I/O authority, and source-incarnation checks. One timeout covers source
admission, server initialization, the request and response processing after the
serialized tool lane is acquired. Queuing is cancellable but is not included in
this budget; blocked synchronous filesystem I/O is not preemptible.

Source changes, same-text close/reopen, a dead connection, or cancellation stop
delivery. A result describes one synchronized source and one server response,
not an atomic snapshot of every dependency or proof of complete indexing.
Explicitly disabled providers stop before request dispatch; missing capability
metadata retains older servers' ordinary request behavior. Provider errors and
timeouts remain errors. Neither navigation nor hover opens a command execution
window or grants unsolicited `workspace/applyEdit` permission.
