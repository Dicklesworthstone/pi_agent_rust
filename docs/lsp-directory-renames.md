# LSP-assisted directory renames

`lsp`'s existing `rename_file` action also accepts a directory. This closes the
single-file-only gap in the OMP-inspired refactoring workflow: ask the language
server for import changes, review them together with the move, and apply one
retained plan through the native edit transaction.

## Review and apply

Start with a preview:

```json
{"action":"rename_file","file":"src/old_module","newFile":"src/new_module","apply":false,"timeout":30}
```

Review `workspaceEdit`, `files`, `server`, `willRenamePairs`, `didRenamePairs`, and
the `directoryRename` summary. Every moved file has a `movedTo` path; the directory
summary also lists the captured directories, including empty ones. A preview
creates no files or destination directories. The usual 200 KiB complete-preview
limit applies; an oversized preview does not receive an approval handle.

Approve the returned identifier without repeating or changing the paths:

```json
{"action":"rename_file","refactorId":"refactor-<returned-id>","apply":true}
```

Approval consumes the exact staged plan without another `willRenameFiles`
request. It checks the original file bytes, permissions, tree membership, live
server and synchronized document evidence. A stale or failed application cannot
be replayed with the old identifier. Existing five-minute refactor expiration
and cancellation behavior applies. Omitting `apply`, or using `apply:true` on a
fresh path request, retains the existing immediate-application behavior.

## Language-server scope

All recognized source files must resolve to **one configured server and one
workspace root**. Selection is deterministic and checked before server startup.
Mixed-language-server trees, nested workspace roots, a workspace root itself,
and the tool working directory are rejected rather than partially refactored.
Both paths must remain inside the working directory and selected workspace.
There must be at least one recognized source file to select a language server;
this action is not a general-purpose move command for empty or asset-only trees.

File-operation registrations are compiled once and honor scheme, glob, case and
`file`/`folder` filters. A matching root-folder registration receives one folder
pair. Otherwise the request contains matching regular-file pairs. `willRename`
and `didRename` filters are evaluated independently. Root and descendant pairs
are not duplicated in one message.

Import coverage depends on the server's registration and returned edits. A server
with no matching registration can still permit a filesystem move, reported with
`willRenameFiles:false` and zero requested pairs. This is **not** a claim that all
references were rewritten. Unknown-extension files are retained and moved, not
opened in a guessed language server.

## Filesystem and failure behavior

The shared WorkspaceEdit transaction stages text edits first, retaining rollback
backups outside the moving tree. It verifies the complete tree and expected final
file images immediately before the final native directory rename. No fallible
edit step follows a successful move. Binary files, hidden/untracked files, empty
directories, and directory permissions are preserved rather than reconstructed
from a source-file list. Changed text-file paths in results refer to their new
locations. `didRenameFiles` is queued only after success; a notification failure
is reported as applied-with-warning, not permission to retry the move.

The tree is bounded to 1,024 total entries and 64 levels. Existing limits remain:
16 MiB per file and 64 MiB of images/replacements per transaction. Request-time
and final prepared snapshots can coexist; these are per-transaction bounds,
not a 64 MiB bound on the entire tool or language-server process.

Symlinks, shared hard links, special files, overlapping source/destination paths,
existing destinations, overwrite/ignore flags, and additional resource operations
in the same batch are refused. The directory rename must be the final operation;
server-supplied create/delete/rename operations cannot be mixed into its import
edit batch. Cross-filesystem moves fail rather than silently switching to a
copy/delete implementation.

Failed import editing or a failed move uses the existing conflict-aware rollback.
Concurrent edits are not overwritten during rollback; incomplete restoration is
reported and recoverable backups are retained. This is not cross-file atomic
visibility, crash recovery, or an OS sandbox against hostile filesystem races.
In particular, the portable native directory rename is not an atomic no-replace
syscall: destination absence is rechecked, but another process can race that
check. Run refactors in a cooperatively owned working tree.

## Validation status

The implementation includes native transaction and public-tool protocol tests.
The latter use an independent Python LSP peer, not a production language server.
Tests were authored but **not executed** during implementation because the required
`dsr quality --tool pi_agent_rust` command was unavailable. Compilation, formatting,
Clippy, production-server interoperability and Windows behavior still require
DSR-backed verification. No full OMP parity or release-readiness claim is made.
