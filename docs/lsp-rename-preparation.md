# Server-confirmed symbol rename targets

Use `prepare_rename` to inspect whether the selected language server considers
a source location renameable. It returns a concrete range, a proposed input
placeholder, and the original source spelling without computing a workspace
edit or modifying files:

```json
{"action":"prepare_rename","file":"src/lib.rs","position":{"line":8,"character":17},"timeout":10}
```

`position` uses zero-based lines and UTF-16 code units, not UTF-8 bytes. It must
be an exact boundary in the file; positions inside surrogate pairs or beyond
the document are rejected rather than clamped. Alternatively, use the existing
`symbol` selector with an optional one-based `line`; `symbol#N` selects a
specific occurrence. Do not combine `position` with `symbol` or `line`.

```json
{"action":"prepare_rename","file":"src/lib.rs","symbol":"old_name#2"}
```

Successful inspection returns `canRename:true` and a `target` containing
`range`, `placeholder`, and `sourceText`. The placeholder is the server's
suggested input text and may differ from the source spelling, for example
when the source name contains language-specific quoting. A bare range result
uses its exact source slice as the placeholder. A cursor immediately after a
token is allowed, but its original position is never replaced by the returned
range start in a subsequent rename request.

A server's null preparation result returns `canRename:false` with a null
target. Provider errors remain errors. Missing preparation support reports
`LSP_UNSUPPORTED`, not a successful claim that the selected symbol is invalid.
Malformed, empty, reversed, unrelated, or nonexact ranges are rejected. This
client negotiates concrete ranges only: it does not advertise the protocol's
`prepareSupportDefaultBehavior` capability or guess an identifier from
language-specific syntax when a server returns `defaultBehavior`.

## Prepare, compute, review, approve

Fresh `rename` requests also accept exact positions. When the server
advertises `renameProvider.prepareProvider:true`, Pi automatically prepares
the target before sending `textDocument/rename`. A null preparation result
fails with `LSP_RENAME_UNAVAILABLE` and no rename request is sent. The new name
is still validated by the language server when it computes the rename.

```json
{"action":"rename","file":"src/lib.rs","position":{"line":8,"character":17},"newName":"new_name","apply":false,"timeout":10}
```

The result includes `prepareRequested`, the confirmed `target`, and the
existing complete workspace-edit preview with a `refactorId`. Approve that
exact staged transaction separately:

```json
{"action":"rename","refactorId":"<preview ID>","apply":true}
```

Approval does not prepare again, recompute a rename, or restage against newer
file contents. The original preview's byte, permission, existence, document
incarnation, and connection checks remain in force. See
[reviewed refactors](lsp-refactor-previews.md) for the approval contract.

An inspection result is **not an approval handle**. Calling `prepare_rename`
and then making a fresh `rename` request checks preparation again under the
new request's source evidence. Do not supply `newName` or `apply` to inspection,
or combine a `refactorId` approval with a position or other replacement
selectors. Inspection itself neither consumes nor extends a reviewed plan.

When preparation is not advertised, ordinary rename still works as before
and reports `prepareRequested:false` with a null target. An explicit
`renameProvider:false` disables symbol renaming. Missing provider metadata is
not treated as proof of preparation support. Fresh rename still applies
immediately when `apply` is omitted or true; use false to review first.
`rename_file` continues to use its separate file-operation workflow.

## Request lifetime and limits

The symbol request retains one bounded source snapshot and synchronized
document incarnation across initialization, preparation, rename computation,
and transaction staging. Source changes or same-text close/reopen invalidate
that evidence. Owner cancellation and filesystem I/O authority are checked
before work and again before commit. Initialization and both protocol calls
share one request budget; each phase does not get a fresh timeout. That budget
begins after the tool's serialized workflow lane is acquired. Waiting for the
lane remains cancellable but is not included in this timeout.

Source admission is limited to 16 MiB; target text, placeholder, and new name
are limited to 16 KiB each. New names must be nonempty and contain no NUL.
Preparation response admission retains the existing 2 MiB limit. Prepared
workspace edits and full previews keep their existing transaction and output
limits. Invalid local positions are rejected before server startup.

Neither preparation nor rename opens a selected-command permission window;
unsolicited `workspace/applyEdit` remains denied. Synchronous filesystem work
cannot be preempted by the timeout. Once commit begins, the existing
finish-or-rollback discipline applies. These checks are not a global snapshot
of dependencies, cross-file atomic visibility, power-loss recovery, or an OS
sandbox against hostile concurrent path replacement.

The protocol regressions use a framed child fixture through the real LSP tool.
Their fixture validates request sequencing and test cases, not real language
analysis. Native Rust execution and real-server acceptance require the
repository's DSR quality lane.
