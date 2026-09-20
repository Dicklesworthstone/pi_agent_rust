# Review and approve workspace refactors

Symbol and file renames can be inspected before any target files change.
A preview retains one immutable, already-staged transaction. Approval commits
those staged final images; it does not ask the language server for another
rename or recalculate edits against newer file contents.

## Symbol rename

Request the rename with `apply:false`:

```json
{"action":"rename","file":"src/lib.rs","symbol":"old_name","newName":"new_name","apply":false}
```

The result includes `applied:false`, `preview:true`, a `refactorId`, the
complete `workspaceEdit`, and a sorted `files` array. Each file entry has
`beforeBytes`, `afterBytes`, and `changed`. A null byte count means the path is
absent at that stage. Unchanged entries include source guards and no-op targets
that also constrain later approval. Positions in the edit are zero-based
UTF-16; the existing symbol selector's optional `line` remains one-based.

Inspect the same plan again without making another server request:

```json
{"action":"rename","refactorId":"<returned ID>"}
```

Approve it explicitly:

```json
{"action":"rename","refactorId":"<returned ID>","apply":true}
```

A selected `refactorId` accepts only its original `action`, `apply`, and
`timeout`. It cannot be combined with file paths, names, positions, ranges,
queries, or other selectors. The preview is the plan being approved, not a
starting point that the approval request can override.

**Fresh requests preserve their existing behavior:** omitting `apply`, or
setting it to true, still applies a fresh symbol or file rename immediately.
Use false to request review. By contrast, a selected `refactorId` writes only
when `apply:true` is explicitly supplied; omitted or false means inspect.

## File moves and imports

```json
{"action":"rename_file","file":"src/old.rs","newFile":"src/nested/new.rs","apply":false}
```

When the server advertises a matching file-operation registration, Pi requests
`workspace/willRenameFiles`. The returned import updates and the final move
are staged together. Preview does not create destination directories, write
scratch files, change imports, move the source, or send `didRenameFiles`.

```json
{"action":"rename_file","refactorId":"<returned ID>","apply":true}
```

Approval applies the combined transaction once. A registered
`workspace/didRenameFiles` notification is written only after successful
application, and `notificationWritten` reports whether that write succeeded.
It is not an acknowledgment that the server has finished indexing the move.
If notification delivery fails after commit, the result still reports
`applied:true`, contains a warning, and retires the connection. **Do not repeat
the move**; reload the language server instead. A rejected approval or failed
transaction does not send this notification.

Servers without a matching registration can still preview and approve the
requested move, but do not supply import updates or receive notifications.
The result exposes `willRenameFiles` and `notificationRequested` so that
absence of integration is not mistaken for updated imports.

## Identity, freshness, and authority

One plan is retained per `LspTool` instance. IDs are opaque and cannot be used
on another instance. A fresh rename replaces the previous plan. Reload,
refactor application, server death, and expiry invalidate old plans. Plans are
usable for five minutes after creation; inspection does not extend that time.
Expiry is checked on use, not by a background eviction task.

Approval checks the original bytes, permissions, existence, and canonical
routes of every staged file, including previously unopened sibling files and
absent destinations. Recreated destinations and changed guard-only sources
cannot silently become new baselines. It also retains the synchronized
incarnations of participating open documents and the exact live server
connection; closing and reopening identical text still invalidates a plan.

Existing request-time source and known-document version/hash checks remain
in force before staging. **Previously unopened files acquire their exact
preimages when the preview is staged after the server responds, not at request
start.** A preview is not a snapshot of every dependency or proof that the
server's analysis is globally up to date.

Each inspection or approval requires the current caller's filesystem I/O
authority and cancellation checks. An earlier request's authority does not
grant a later caller permission. Invalid overriding selectors, foreign IDs,
and a mismatched rename action do not consume the matching valid plan.
Once a matching, admitted selection begins validation, failures retire it;
applications consume it before any write. Reusing an applied or retired ID
returns `LSP_REFACTOR_STALE` without another server request. Correct the
conflict and obtain a new preview rather than retrying the old ID.

Neither preview nor approval opens a server-command edit permission window.
Unsolicited `workspace/applyEdit` requests remain rejected. Approval applies
only the reviewed local transaction, not arbitrary command side effects.

## Bounds and failure semantics

The complete preview, including the full workspace edit and file metadata,
must fit within 200 KiB of serialized JSON. Larger previews fail with
`LSP_EDIT_LIMIT` and do not issue an approval handle; there is no approval of
silently omitted edits. The existing transaction limits still apply: 16 MiB
per file, 64 MiB of snapshot/replacement accounting, and 1,024 loaded paths.
Only one staged plan is retained, and replacement or consumption releases it.

The transaction uses the existing regular-file validation, permission
preservation, preimage rechecks, and rollback-on-reported-error mechanism.
Directories as edit targets, symlinks, shared hard links, special files, and
file/directory shape changes remain unsupported. Incomplete rollback remains
an explicit `LSP_EDIT_ROLLBACK` error with recovery information, and the plan
is consumed even on that failure.

`atomic:false` is deliberate: this is not cross-file atomic visibility,
power-loss recovery, or an OS sandbox against hostile concurrent directory
replacement. Synchronous filesystem I/O cannot be preempted by the approval
timeout or cancellation checks. Once commit begins it follows the existing
finish-or-rollback discipline rather than abandoning a partially changed
workspace.
