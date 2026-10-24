# Model-assisted terminal handoff

Both terminal frontends can compact the current session into a model-generated
continuation brief:

```text
/handoff
/handoff preserve migration decisions and remaining verification
```

This uses the same native operation as print-mode `/handoff`: it saves a
compaction entry in the current session, preserves the original branch history
and recent complete turns, and gives the next prompt the brief plus the retained
tail. Automatic compaction can remain disabled. There must be an older safe
prefix outside the configured recent-history budget; a single authored turn or
a budget retaining all history is refused before any model request.

The operation uses the selected provider, configured privacy screening,
compaction hooks and durable session transaction. Failed, empty, truncated or
tool-requesting responses do not produce a fallback brief. Escape requests
cancellation while the UI keeps the operation busy until the native transaction
returns. A cancellation after durable installation does not undo the saved
entry. Each queued request keeps its original session identity and cancellation
handle, so switching sessions cannot retarget it. Focus accepts at most 4096
UTF-8 bytes and no NUL; `/handoff -- <focus>` permits a leading dash.

Use `/handoff --report [human|agent|target] [path]` for the existing local
structured report or delivery workflow. Unrecognized leading options are
refused. The `pi handoff` subcommand remains a local report command.

The default terminal also supports `/handoff --import <absolute-bundle.json>`
to inspect an exported evidence bundle without calling a model. Supplying the
explicit `--session-output` and `--workspace` destinations creates a separate
native continuation session. Import keeps the active session unchanged and
requires an explicit resume before further work. See
[Handoff import](handoff-import.md) for the input validation and command syntax.

## Separate model preview and export

The default terminal can also synthesize a standalone evidence-based preview
with its currently selected model:

```text
/handoff --model
/handoff --model -- focus on remaining verification and the unanswered deployment question
/handoff --model --export "/tmp/pagination-handoff.json" -- remaining verification
```

This is an explicit, one-shot auxiliary model request. The `--model` form
displays a preview and writes a local bundle only when an explicit
`--export` destination is supplied. It never sends mail or Bead comments,
creates/switches sessions, changes a goal, or resumes execution. Recipient
arguments are not accepted by the model form.

## What the preview contains

The brief contains the goal, current state, decisions, failed approaches,
open questions/blockers, and recommended next steps. Every generated item must
reference one or more supplied evidence IDs. IDs are positions on the selected
session branch; the source map labels user requests, assistant claims, tool
results, Bash results, and compaction summaries separately.

Source-ID validation proves only that the referenced records were supplied.
It is **not** a semantic proof that the model's claim follows from those records.
Assistant claims and summaries are not independent test or filesystem evidence.
Review the brief and actual tool results before a successor acts.

Only the selected branch is read. Other branches and abandoned attempts are not
merged into the successor's task. The first user request and newest compaction
summary are considered as anchors, followed by recent complete records. Up to
256 recent candidates are examined, at most 64 records are sent, each record
must fit 16 KiB encoded, and total evidence fits 48 KiB. Oversized records are
omitted whole, not sliced into misleading prefixes. The brief reports its
coverage and omission count, including non-conversational entries.

Image/audio/video bytes, thinking blocks and provider thought signatures are
not sent as text. The brief cannot claim visual understanding of an omitted
attachment. Session traversal itself uses the existing branch implementation;
these payload limits are not a new preemptible sandbox for a corrupt session.

## Portable evidence bundle

`--export` creates one JSON file with schema `pi.handoff.bundle.v1`. The file
contains the structured `brief`, its rendered `markdown`, the selected redacted
`source.evidence` records, the screened focus, coverage/omission counts, model
identity, auxiliary reported usage and generation timestamp. A successor can
resolve the brief's numeric source references against those actual records
without access to the original session tree. The original conversation remains
unchanged; this is not a session backup or a replayable session file.

`sourceSnapshotDigest` is a SHA-256 digest of the reviewed source identity and
selection. It binds the generation to that snapshot without exporting raw
session identifiers. It is not a signature, authentication, or proof of the
model's interpretation. `executionAuthorized` is always false. No provider
credentials, cache keys, tool permissions or automatic continuation are bundled.
The file can still contain private project information: review it before sharing.

Supply an **absolute path** whose parent directory already exists. Paths with
spaces or quotes use JSON string quoting, not shell expansion; on Windows,
backslashes inside quoted paths must be escaped as JSON, or use forward slashes.
No parent directories are created. An existing file, directory or symlink,
including a dangling symlink, is refused before the paid model request.
Unquoted paths without whitespace are also accepted. The optional focus follows
`--`; extra options, recipients and a second output path are errors.

Publication uses a size-bounded temporary file in that same parent directory.
On Unix it explicitly sets mode 0600. The complete file, capped at 256 KiB, is
written and synced before publication. The source session is locked, its branch
snapshot is rechecked, and the final no-replace publication occurs without an
intervening await. A conflicting destination created during generation is never
overwritten. Invalid model output, stale source or refusal before publication
drops the staged file instead of creating an incomplete final bundle.

The underlying `tempfile::NamedTempFile::persist_noclobber` operation guarantees
no replacement, not universal filesystem crash atomicity. On some filesystems or
an interrupted cleanup it can leave the temporary hard link behind. Directory
metadata is not synced here, so survival across power loss is not guaranteed.
Temporary cleanup is best effort. Parent-path rechecks are not a sandbox against
a hostile process replacing directory entries; select a directory you control.
A successful publication is not later reported as absent merely because the
owner becomes cancelled after it. Export does not deliver the file anywhere.

## Model, privacy and failure behavior

The request uses the active session's provider and credential options, with no
fallback or tool definitions. It gets a fresh transport session ID and does not
reuse the parent's prompt-cache key. The existing auxiliary completion service
owns the 60-second request budget, owner cancellation, terminal-event validation
and output limits. The request asks for at most 4096 output tokens and still
respects a smaller configured host ceiling. Auxiliary reported usage is displayed
separately; it is not silently charged to the main conversation's token counters.

The owning agent's auxiliary privacy projection screens the bounded input before
provider admission, including configured secret patterns and remembered secrets.
The response and selected source are screened together before display or export,
so echoed source secrets do not become a raw preview or a raw evidence appendix.
Source IDs, source kinds and coverage are revalidated after screening. Block-mode
refusal remains refusal. The omitted parts of a large transcript are not rescanned
by this command. Export requires the initiating owner's I/O capability and checks
cancellation before file staging and final publication.

A local JSON Schema requires the complete output shape, bounded sections and
valid source IDs. Duplicate keys, markdown wrappers, extra fields, unknown source
IDs, missing terminal events and failed provider responses are rejected. There
is no repair request, heuristic fallback or partial successful brief. Changed
identity, leaf or selected evidence produces `HANDOFF_STALE`. The final source
lock prevents mutation during publication, not edits after a completed export.

The `--model` preview/export flow retains its existing owner cancellation and
bounded auxiliary deadline. The request-specific Escape handling described
above applies to in-place `/handoff [focus]`.

## Validation

The original eight controlled-provider/native-dispatcher tests remain in place.
Eight additional Rust tests cover quoted-path parsing, actual dispatcher export,
branch-local evidence and unchanged history, private file permissions, credential
screening, occupied/missing-parent refusal before provider admission, failed and
stale generation cleanup, destination races, payload/identity bounds, and Unix
symlink refusal. The publication-race test uses real filesystem operations; the
model responses are controlled-provider fixtures, not live-model evidence.

These tests are authored, not executed on the implementation host. The required
`dsr quality --tool pi_agent_rust` command was attempted and failed to start with
`dsr: command not found` (exit 127). No compilation, Rust test, Clippy, formatting,
live-provider, PTY, Windows or cross-platform acceptance is claimed. The default
terminal's goal keyboard-control integration remains unfinished.

Interactive in-place regressions additionally cover both native terminal routes,
same-session persistence and context refresh, explicit report/preview routing,
extension-command precedence, pre-dequeue cancellation, stale request replies,
background notices, late pickers, busy drafts and classic display-lock retries.
These regressions are also source-only until the DSR quality lane runs; static
syntax review is not compiler or test evidence.
