# Enclosing selections for refactoring

`selection_ranges` asks the configured language server for progressively larger
selections around a source cursor. This helps choose the expression, statement,
or enclosing construct to pass to a selected-range code action, without Pi
inventing language-specific token or syntax-tree boundaries.

```json
{"action":"selection_ranges","file":"src/lib.rs","position":{"line":8,"character":17},"limit":16,"timeout":10}
```

The existing `symbol` selector, optional one-based `line`, and `symbol#N`
disambiguation can be used instead of `position`. Mixed selectors and invalid
local positions fail before server startup. Positions and returned ranges use
zero-based UTF-16 coordinates, not UTF-8 byte offsets.

The action requires a statically advertised `selectionRangeProvider`. Missing
or disabled support reports `LSP_UNSUPPORTED`; there is no guessed selection
fallback and no dynamic registration support is claimed.

## Inspect, select, review, apply

The `ranges` array is ordered **inner to outer**. Each entry includes its
one-based `index`, exact `range`, `byteLength`, `empty`, and a source `text`
preview. `textTruncated` says whether that preview was shortened; the range is
always complete. The server may suggest equal ranges at adjacent levels; their
order is preserved. An empty range at the cursor is the protocol's explicit
no-selection result, not an expression inferred by Pi.

Pass a chosen nonempty `range` unchanged to a fresh code-action request:

```json
{"action":"code_actions","file":"src/lib.rs","range":{"start":{"line":8,"character":10},"end":{"line":8,"character":25}},"only":["refactor.extract"]}
```

Use the actual returned coordinates, not the example above. Select the returned
`actionId` without `apply:true` to resolve and preview the edit-only action;
then approve its exact `refactorId` with `action:"code_actions", apply:true`.
Code actions still determine whether extraction is available and may affect
other files. Command-backed actions retain their separate explicit execution
path and cannot be promised as complete static previews.

A selection result is **not a retained approval handle**. The later code-action
request owns a fresh source snapshot, so changes between the two requests are
not detected by a selection handle. Refresh the selection after editing the
source and review the resulting action before approval. This feature does not
claim that a server suggestion is an AST node, an extraction guarantee, or an
atomic snapshot of dependencies.

## Bounds, validation and permissions

`total` counts all validated levels; `count` counts returned levels.
`truncated` and `responseComplete` describe range coverage, independently of
per-entry `textTruncated`. At most 64 levels are accepted, with 16 returned by
default. Each source preview is at most 4 KiB, clipped at a UTF-8 boundary, and
the entire output is at most 200 KiB including JSON escaping. Only complete
range records are returned. Omitted parents are still validated.

The source and server response retain the semantic workflow's 2 MiB limits.
The response must be null or contain exactly one selection for the requested
cursor. Every range must have exact, ordered, in-bounds source coordinates;
the cursor must be inside it, and every parent must contain its child. Reversed
ranges, surrogate interiors, missing ranges, unrelated parents, incorrect
cardinality and malformed parents fail rather than becoming partial success.
`returnedNull` distinguishes no server report from an explicit empty selection.
Array partial results use the existing request-owned collector and cannot
silently disappear or supply extra selections for this single-cursor request.

One timeout covers source access, initialization, request and processing after
the tool's serialized lane is acquired. Owner cancellation and I/O authority,
source bytes, synchronized document incarnation, and connection liveness are
rechecked using the same machinery as navigation. Source changes and identical-
text close/reopen invalidate a pending result. Synchronous filesystem I/O is
not preemptible and the queue wait is cancellable but outside this timeout.

Inspection makes no file edits, issues no command, grants no server edit lease,
and does not consume or approve cached refactor plans. Returned text is source
data, not an instruction or executable snippet.
