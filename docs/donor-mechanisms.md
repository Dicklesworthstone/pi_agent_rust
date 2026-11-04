# Adopted source-search and recovery mechanisms

This change adapts selected mechanisms from the owner's local Jev and NVCD
projects into Pi's existing owners. It adds no donor runtime or dependency.

The final review also covers the owner's Rust Jev, cef-rs, graphify-rs and
codegraph-rust snapshots. The graph donors contribute bounded syntax outlines
and explicit evidence provenance to the same source-search operation. Rust Jev's
typed judgment engine overlaps Pi's existing service; its logprob scoring and
temperature calibration require provider capabilities Pi does not currently
promise, so no calibrated-probability claim is introduced. CEF's browser runtime,
native bindings and platform bundles would duplicate Pi's managed Chromium/CDP
owner; they are deliberately not imported. Pi retains its sandbox policy and
owned-process cleanup.

### Selection from the four Rust donors

| Donor and inspected source | Valuable mechanism | Final disposition |
|---|---|---|
| jev-rs: `src/judge.rs`, `src/score.rs` | Typed distributions, derived scores, explicit calibration | Reuse Pi's existing typed Judge; keep search scores explicitly uncalibrated. No duplicate scoring backend. |
| cef-rs: `examples/cefsimple/src/shared/mod.rs`, workspace manifest | Browser process dispatch, initialization/shutdown, platform sandbox packaging | Retain Pi's existing owned Chromium/CDP lifecycle. No CEF bindings, embedded UI or runtime download. |
| graphify-rs: `crates/graphify-extract/src/treesitter/handlers.rs`, `crates/graphify-core/src/model.rs`, `confidence.rs` | Syntax nodes, containment, source locations, extracted versus inferred provenance | Implement bounded source outlines and provenance in the existing search results. |
| codegraph-rust: `crates/codegraph-parser/src/visitor.rs`, `crates/codegraph-mcp-tools/src/graph_tool_executor.rs` | Scope-aware source navigation, bounded evidence, staged search/reranking | Use Pi's existing parser for declaration ranges and nesting before full-source judgment. Retain Pi's existing LSP for resolved navigation. |

These are mechanism adaptations, not vendored donor implementations. Full graph
databases, embeddings, community detection, graph servers, background watchers
and persistent semantic caches are excluded: they introduce independent state,
providers and freshness/privacy contracts beyond the bounded search operation.
No donor benchmark or accuracy claim is transferred to Pi.

## Source search

See [typed judgments](judgment.md#semantic-source-search-and-predicate-audits)
for the `__pi_search` eval operation. It uses Jev's declaration screening,
full-source verification and location stages, with explicit partial coverage.
Predicate mode shares that pipeline and reports unknown and failed outcomes.
Pi's model binding, tool policy, source privacy and cost receipts remain in force.
The donor's reported precision and token savings have not been established for Pi.

Source screening now uses the existing AST grammar registry when the complete
privacy-projected numbered source remains parseable. Each returned `structure`
contains at most 64 declarations/imports, their one-based source ranges and
parent IDs describing syntax containment. IDs are local to that file's result.
These nodes are tagged `EXTRACTED`; model judgments are tagged `INFERRED`.
This is file-local syntax evidence, not resolved calls, dependency analysis or
proof of runtime behavior. Syntax errors or excess declarations set `partial`
on the structure. Unsupported languages, damaged line labels, parse errors and
bounded outlines fall back to textual screening; full-source verification and
the search coverage rules still apply. No persistent graph or cache is created.

## Validation evidence

The validation broker now snapshots current workspace bytes and the immediate
validation executable when acquiring a slot. Making a slot reusable requires
the identity to match again; reuse also checks the current identity. Two dirty
states at the same Git HEAD therefore cannot share the same recorded result.
Legacy or unavailable identities cannot coalesce as reusable evidence.

The bounded snapshot covers the cwd tree except `.git`, the declared target
directory and temporary directory. Put validation logs and broker stores in
those output directories or outside cwd. Symlinks, unreadable inputs, more
than 100,000 entries, or more than 512 MiB make identity unavailable. Bare
executables require the request's explicit `PATH`; wrapper subprocesses,
remote workers and dependencies outside cwd still need DSR-owned attestation.
This mechanism improves evidence identity; it does not replace DSR validation.

## Restart undo

Default persistent CLI and SDK sessions open a private journal in the agent's
global `undo` directory, keyed by session identity. Ephemeral sessions and
explicit caller-supplied recorders retain their existing behavior. Journals
are scoped to the original canonical workspace roots and held by one process.
FTUI reloads share the same session's held recorder; new sessions, forks and
switches to another saved session obtain that session's own journal by default.

The journal retains bounded content-addressed backups, validates every blob
hash and an outer checksum, snapshots pending work before writes, and records
undo/redo intent before restoration. Pending edits without a committed post-state
become recovery gaps. Interrupted restoration, corruption, different workspace
scope, or changed parents fail closed and require inspection. Journal write
failures appear in recorder statistics and block further replay rather than
claiming durable success. Ordinary external edits still block unforced undo.
An inaccessible, corrupt, busy or mismatched journal does not block opening
the conversation: default session startup warns and exposes disabled undo/redo
through recorder statistics and the undo command, preserving the existing
journal for inspection. Explicit `open_journal` callers still receive an error.
Changing the workspace root set can therefore require manual undo recovery;
the previous journal is never silently accepted under broader authority.
Pending creation of missing nested parents remains an unreplayable gap after
restart. Saving enforces the same backup-byte and file-count bounds as loading;
an oversized edit cannot overwrite the last recoverable journal with a snapshot
that the loader would reject. Edits outside the journal's original roots also
disable durability before replacing that journal.

Backups contain original file contents, including any secrets present there;
the journal uses private files under agent state. Unix journal files must be
0600 with one hard link. Windows inherits the private agent-directory ACL.
The existing 64 MiB backup budget remains; at most 4,096 journal units/files
are admitted. Atomic journal snapshots add disk work to recorded edits.

Parent identifiers are historical checks for ordinary replacement, not proof
against hostile inode/file-index reuse after a process restart. Multi-file
restoration remains a sequence of atomic file replacements. Unsupported LSP
and child-agent mutation sources remain visible gaps. Retention does not delete
backup files automatically, and no automatic reconciliation replays uncertain work.

## Windows and other non-Linux resource sampling

The existing resource governor uses installed `sysinfo` APIs to sample total RAM,
Pi RSS and process count. Live admission also accounts for host RAM pressure
from other processes. Explicit operator caps remain upper bounds. Missing or
invalid observations preserve the conservative unknown fallback; Linux `/proc`
sampling is unchanged. This adds admission measurements, not a model runtime
or a claim of Windows sandbox isolation.

## Verification

The patch includes focused source-level regression tests for these behaviors.
The required qualification command is `dsr quality --tool pi_agent_rust`.
No direct Cargo command is an alternate gate. A patch whose application check
passes still needs DSR's compiler, formatter, lint and runtime test evidence.
