# Native semantic inspection

`signature_help` asks the configured language server about the callable at an
exact cursor position. This complements completion: a snippet supplies syntax,
while a signature report supplies overloads, parameter labels and documentation.

```json
{"action":"signature_help","file":"src/main.rs","position":{"line":8,"character":17},"limit":16,"timeout":10}
```

Place the cursor inside the call's argument list. Positions are zero-based lines
and UTF-16 code units, including the two units occupied by a supplementary Unicode
character. Invalid positions are rejected before starting a server. Symbol names
and one-based `line` selectors are not substitutes for this cursor.

## Signature results

`signatures` contains the server's labels, documentation, normalized parameter
labels, optional UTF-16 `labelOffsets`, and zero-based original `index` values.
`activeSignature` identifies the server-selected overload by its original index;
`activeParameter` identifies the selected parameter of that overload. Per-signature
active-parameter information takes precedence over the top-level value. Omitted
or out-of-range active indices use LSP's zero default where an item exists; a
zero-argument signature has no active parameter. Offsets in parameter labels are
never clamped through a surrogate pair or beyond the signature text.

At most `limit` signatures are returned (default 16, maximum 128). The active
overload remains present even when it is not in the first `limit` entries. Original
indices are retained, so use `index`, not the position in the returned array, when
interpreting `activeSignature`. `total` and `truncated` describe omitted overloads.
Malformed omitted overloads still fail validation rather than being hidden by a
small output limit.

A null report or an explicitly empty signature list is a successful absence of
signature help. Missing capabilities, provider errors, malformed responses and
expired requests remain errors; they are not converted to empty success.

## Ownership and freshness

This operation is read-only: it does not execute commands, apply edits, follow
links in documentation or grant `workspace/applyEdit` permission. The LSP tool as
a whole still declares write/process effects because its other actions can edit
files and launch language servers. `apply` and unrelated action selectors are
rejected here.

The request retains exact source text and its synchronized document incarnation.
Source changes during startup, the request, or result processing cause
`LSP_SEMANTIC_STALE` instead of returning locations tied to different text.
Close/reopen with identical bytes also retires the incarnation. This protects the
queried file, not a dependency-wide snapshot of the project. Server analysis can
still change when other files or dependencies change.

The caller's timeout covers source admission, server startup, the protocol request
and result processing. Cancellation uses the existing client request owner.
Synchronous filesystem operations cannot be preempted by this budget; this is not
a hard wall-clock guarantee against a blocked filesystem. Regular nonsymlink
source files are required. Path checks are not a sandbox against a hostile
concurrent filesystem renaming process.

Sources are bounded to 2 MiB, server payload admission to 2 MiB, signature counts
to 128, parameters per signature to 128, and individual labels to 16 KiB. Final
structured output must fit the existing 200 KiB tool limit. Oversized output is
an error: narrow the request rather than treating a truncated JSON string as a
complete semantic report.

Protocol fixtures are not live-server certification. Native compilation and tests
must be executed through `dsr quality --tool pi_agent_rust` before claiming native
validation.

## Inferred type and parameter hints

`inlay_hints` exposes the type annotations, parameter names and other inline
semantic labels supplied by the language server. It does not infer them locally.
Omit the range to inspect the whole file, or select exact zero-based UTF-16
boundaries to focus the response:

```json
{"action":"inlay_hints","file":"src/main.rs","range":{"start":{"line":4,"character":0},"end":{"line":20,"character":0}},"limit":50,"timeout":10}
```

The output keeps hint order, including multiple hints at the same position.
Each hint has an original `index`, exact `position`, normalized `kind` (`type`,
`parameter` or `unspecified`), combined `label`, optional `labelParts`, tooltip,
and padding flags. Label-part tooltips and source locations remain available
as metadata; external URIs are not opened, fetched or interpreted as local paths.
Hints are zero-width anchors and may touch either selected endpoint, including
the exact document end. Out-of-range or malformed positions are errors, not
silently clamped or dropped.

`resolve:true` requests lazy tooltips and label locations for the retained hints:

```json
{"action":"inlay_hints","file":"src/main.rs","limit":20,"resolve":true,"timeout":10}
```

Resolution requires the server's `resolveProvider` capability. Inline-only
providers still work when `resolve` is omitted or false. The resolver receives
the exact original hint and its opaque data, not the normalized output. Only
`tooltip`, `label.tooltip` and `label.location` may change: substituting a label,
position, kind, command, text edit or other identity-bearing data fails the
request. Resolved rows carry `resolved:true`. No hint cache or long-lived handle
is created, and every request checks the current source again.

The same source-incarnation, caller-authority, cancellation and total request
budget used by signature inspection cover listing and all resolve calls. An
error during resolution fails the request instead of returning unresolved or
stale rows as successful detailed results. Selecting fewer hints reduces the
number of resolver requests; omitted hints are still structurally validated.

**This is inspection, not acceptance.** `textEditsPresent` and label-part
`commandPresent` report that the server attached these optional operations.
Their payloads are not returned or applied. Neither listing nor resolving a
hint grants a server `workspace/applyEdit` permission window. `apply`, cursor
`position`, and selectors belonging to other actions are rejected. Use the
explicit existing completion, code-action or editing workflows for changes.

Hint responses admit at most 4,096 server items, with at most 128 returned
(default 50) and optionally resolved. Each raw hint is limited to 64 KiB, its
combined label to 16 KiB, and its label parts to 64. The existing 2 MiB source
and response limits and 200 KiB final output limit also apply. Cumulative
resolved output is checked after each item so an exhausted result budget does
not cause unnecessary later resolver calls. `total`, `count` and `truncated`
distinguish omitted hints from an empty report. Narrow the range or limit when
detailed documentation exceeds the output budget.
