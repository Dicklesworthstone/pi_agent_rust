# LSP-assisted directory renames

`lsp`'s existing `rename_file` action also accepts a directory. The OMP-inspired
refactoring workflow requests language-server import changes, reviews them
together with the move, and applies one retained native edit transaction.
Mixed-language trees can now coordinate multiple servers in the same workspace.

## Review and apply

Start with a preview:

```json
{"action":"rename_file","file":"src/old_module","newFile":"src/new_module","apply":false,"timeout":30}
```

Review `workspaceEdit`, `files`, `servers`, `willRenamePairs`, `didRenamePairs`,
and the `directoryRename` summary. Every moved file has a `movedTo` path; the
directory summary includes empty directories. A preview creates no files or
destination directories. The complete preview must fit 200 KiB; an oversized
preview does not receive an approval handle.

Approve the returned identifier without repeating or changing the paths:

```json
{"action":"rename_file","refactorId":"refactor-<returned-id>","apply":true}
```

Approval consumes the exact staged plan without another `willRenameFiles`
request to any server. It checks original file bytes, permissions, tree
membership, every participating connection and synchronized document evidence.
A stale or failed application cannot reuse the identifier. The existing
five-minute expiration, owner cancellation and annotation-confirmation rules
apply. Omitting `apply`, or setting `apply:true` on a fresh path request,
retains immediate application, except that edits requiring annotation
confirmation must first be previewed and approved.

## Coordinated language-server scope

Each recognized source file selects its configured server using the existing
extension registry. Up to **eight distinct selected servers** may participate,
but all must resolve to the **same canonical workspace root**. All selection
and root checks happen before startup; all participants are then synchronized
before the first import-edit request. There is no partial-success fallback
when a selected server cannot start or rejects preparation. One timeout and
owner cancellation budget cover the complete workflow, not a new budget per
server. Requests are sequential and ordered by server name.

This is not a fanout to every configured server advertising the same extension:
the registry still selects one definition per file. Nested/different workspace
roots, a workspace root itself and the tool working directory remain rejected.
Both paths must stay inside the working directory and selected workspace.
At least one recognized source file is required; this is not a general-purpose
move command for empty or asset-only trees. Unknown-extension files are moved
unchanged, not opened in a guessed language server.

File-operation registrations honor scheme, glob, case and `file`/`folder`
filters independently for each server. A matching root-folder registration
receives one folder pair; otherwise only matching regular-file pairs are sent.
`willRename` and `didRename` are evaluated independently. Root and descendant
pairs are not duplicated within one message.

`servers` reports each selected server and its requested pair counts;
`serverCount` reports participation. `server` remains the name for a single
server and is null for multiple servers. Aggregate pair counts can include
the same path sent to different servers. A participant with no registration
has zero pairs: it does **not** establish that its references were rewritten.
Import coverage always depends on the servers' registrations and responses.

## Reconciling independent import edits

For multiple contributors, each response's document versions are checked
against its own connection before merging. Version numbers are not portable
between servers; the combined edit uses null versions, while native request
snapshots and immutable file preimages remain the approval authority.

Nonoverlapping edits apply against one original document, not successively
shifted coordinates. Exact duplicate edits with identical annotation metadata
are applied once and counted in `deduplicatedEdits`. Overlapping replacements,
ambiguous insertion boundaries, different replacements for one range, or
identical text with different annotations fail with `LSP_RENAME_CONFLICT`.
No server is silently preferred. Each server's annotation IDs are namespaced,
so unrelated `needsConfirmation` requirements survive collisions.

Independent responses cannot contain sequential coordinate spaces for the
same file or additional create/delete/rename operations. Those cannot be
safely reconciled as simultaneous imports. Single-server requests keep their
existing ordered-edit behavior. Aggregate responses are limited to 2 MiB,
32,768 edits and the native transaction/annotation limits; exceeding a limit
is an error before any application.

## Filesystem and failure behavior

The shared transaction stages text edits first and retains rollback backups
outside the moving tree. It verifies the full tree and expected final file
images immediately before the final native directory rename. No fallible
edit step follows a successful move. Binary, hidden/untracked files, empty
directories and directory permissions are preserved by that native move.
Changed text-file paths in results refer to their new locations.

`didRenameFiles` is queued to every requesting participant only after success.
`serverNotifications` records individual queue outcomes. A failed notification
does not prevent attempts to notify other participants or turn an applied move
into a retryable error. `notificationWritten` means queue acceptance for all
requested notifications, **not server acknowledgement or adoption**.

The tree is bounded to 1,024 entries and 64 levels; files remain limited to
16 MiB and transaction images/replacements to 64 MiB. Request-time and final
prepared snapshots can coexist: these are not whole-process memory bounds.
Symlinks, shared hard links, special files, overlapping source/destination
paths, existing destinations, overwrite/ignore flags and nonfinal directory
moves are refused. Cross-filesystem moves fail without a copy/delete fallback.

Failed edits or a failed move use conflict-aware rollback. Concurrent edits
are not overwritten during rollback; incomplete restoration is reported and
recoverable backups are retained. This is not cross-file atomic visibility,
crash recovery or an OS sandbox against hostile filesystem races. Portable
native directory rename is not an atomic no-replace syscall: destination
absence is rechecked, but another process can race that check. Use a
cooperatively owned working tree.

## Validation status

Native transaction, merge and public-tool protocol tests are included. The
protocol tests use independently running Python peers, not production language
servers. Authored Rust tests were **not executed** during implementation: the
required `dsr quality --tool pi_agent_rust` command was unavailable. Compilation,
formatting, Clippy, production-server interoperability and Windows behavior
still require DSR-backed verification. No full OMP parity or release-readiness
claim is made.
