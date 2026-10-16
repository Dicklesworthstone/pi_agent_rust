# Lossless conflict inspection and resolution

`read` resolves `conflict://*`, `conflict://0`, and side views such as
`conflict://0 @ours`, `@theirs`, or `@base`. Region numbers are zero-based and
ordered by repository-relative path, then byte position. Running from a nested
working directory still inspects the Git worktree root.

The index identifies unmerged files; Pi parses their current working bytes.
Two-way, diff3 and Git's longer/custom-width conflict markers are recognized.
Untouched regions, Unicode, a UTF-8 BOM, CRLF and a missing final newline stay
byte-for-byte unchanged. A missing base section is an error, not an empty
replacement. Malformed or incomplete marker blocks fail inspection rather than
letting an apparently valid prefix authorize a destructive edit.

Each single-region read includes a SHA-256 `token` binding the repository,
relative path, complete source bytes and region span. Numbering alone is not a
stable identity: resolving an earlier region can renumber later ones. A token
is a preimage fingerprint, not a credential or permission grant.

The existing Rust API `url_router::write_conflict_resolution(cwd, index, side)`
takes a fresh snapshot and replaces only that selected span. It preserves
permissions and all other file bytes. It does **not** stage the Git index,
resolve binary/submodule conflicts, create a commit, or run a merge command.
The ordinal-only API cannot detect a choice made against an older read.

Publication reuses Pi's staged workspace-edit transaction. It validates
preimages before mutation and rechecks them at commit. Reported failures use
that transaction's existing rollback/recovery behavior; this is not a
crash-atomic transaction or a filesystem sandbox against arbitrary hostile
concurrent path replacement. The operation budget gates publication but does not
interrupt the synchronous transaction once entered. Source reads are descriptor-relative, no-follow
and nonblocking on Unix; non-Unix reads have weaker path-race guarantees.

Git inventory requests share a maximum ten-second operation budget, capped by
the caller's inherited deadline and cancellation. Output capture uses temporary
files rather than blocking pipes, and refuses inventories above 1 MiB. The
polling bound is not a hard instantaneous limit on temporary disk growth.
Inspection admits at most 256 unmerged files, 1024 regions, 4 MiB per UTF-8 file,
and 16 MiB of source bytes. It fails on exceeded bounds instead of presenting a
partial inventory as complete. Non-UTF-8 names are rejected rather than guessed.
An unmerged file without text markers still counts as unmerged, even though it
has no text region this API can resolve. Normal `read` result pagination and
truncation still apply to the resulting document.

## Validation

Added parser and Git-worktree regressions for multiple regions, diff3, custom
widths, CRLF/BOM/Unicode, final-newline preservation, stale source bytes,
subdirectory callers, unusual filenames, executable permissions, unsafe links,
malformed markers and resource limits. The multi-region case addresses a prior
resolver which skipped every marker block while selecting only one replacement.

These Rust tests are authored, not executed in the implementation environment.
`dsr quality --tool pi_agent_rust` could not start because `dsr` was unavailable.
Compilation, formatting, Clippy and native runtime acceptance remain unverified.
