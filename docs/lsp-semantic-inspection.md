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
