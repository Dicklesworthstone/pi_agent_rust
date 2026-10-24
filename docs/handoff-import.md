# Import an evidence-bearing handoff

The default terminal can inspect a `pi.handoff.bundle.v1` file produced by
`/handoff --model --export`. Inspection is local and provider-free:

```text
/handoff --import "/absolute/path/handoff.json"
```

This validates the bundle, screens it with the current agent's configured
privacy policy and remembered secrets, and displays a regenerated brief with
its source records. It neither appends to the current conversation nor sends
anything to a model. The supplied Markdown and warning fields are not trusted
as the presentation: the importer renders the validated structured brief.

## Create a separate resumable session

Explicitly choose BOTH a new session file and its intended workspace:

```text
/handoff --import "/absolute/path/handoff.json" --session-output "/absolute/path/continuation.jsonl" --workspace "/absolute/path/project"
```

The workspace must already exist and be readable. The output's parent must
already exist, the output must end in `.jsonl`, and the output must not exist.
Every path must be absolute. Paths with spaces use JSON string quoting, not
shell expansion; Windows backslashes inside quotes need JSON escaping.
Options use the order shown. No directories are created.

After inspecting the result, open the new file explicitly:

```sh
pi --session /absolute/path/continuation.jsonl
```

Import does not switch or discard the current session. The new session has a
fresh identity and the explicitly selected canonical workspace. It contains
one native `handoff` custom message with the regenerated brief, selected source
evidence and an untrusted-history warning. Source tool calls, tool results and
Bash commands are JSON text inside that message, not native executable records.
No current-user prompt is synthesized, and no historical request is replayed.
Normal resume-time configuration, provider selection, workspace trust and tool
policies still apply. The bundle cannot choose those policies or a workspace.

The new file is not a full transcript backup. It carries only the exported
selection; omitted entries, media payloads and reasoning are not recovered.
Review it and provide a new request rather than treating proposed next steps
as standing authorization. The warning is not a prompt-injection sandbox.

## Validation, privacy and provenance

The reader accepts exactly one JSON value, rejects duplicate keys including
escaped duplicates inside tool arguments, and enforces byte, nesting and node
budgets using the existing strict completion decoder. Unknown bundle fields,
versions, evidence kinds, invalid source references and inconsistent coverage
are errors. Evidence IDs must be ordered, unique, positive and within the
claimed branch size. `executionAuthorized` must be false. Each brief item must
satisfy the same host-generated schema used by the exporter.

The exporter bounds remain in force: at most 64 complete evidence records,
16 KiB per encoded record, 48 KiB total evidence and 128 text/call blocks per
record. Input and output artifacts are capped at 256 KiB. Privacy screening
sees the whole admitted bundle and validation runs again afterward. Screening
may refuse input or invalidate its structure; neither outcome falls back to
unscreened content. Detection is not a guarantee that every private value is
recognized, so review the file before sharing or resuming elsewhere.

The SHA-256 of the imported file identifies its exact bytes. The exported
source-snapshot digest, model and timestamp remain explicitly claimed metadata;
the original source is unavailable to verify them. Neither digest authenticates
the author or proves a model interpretation. Referencing supplied evidence
proves membership, not correctness or successful execution.

## File ownership and failure behavior

A source must be a regular file, not a symlink or special device. Reads are
bounded even if the file grows. Unix opens use no-follow/nonblocking flags and
check descriptor identity against the initial metadata; all platforms check
size and modification metadata around the read. These checks are not a hostile
filesystem snapshot service, and non-Unix opens have weaker race protection.
A filesystem read may block; owner checkpoints do not preempt kernel I/O.

Creation reuses the exporter's same-directory private temporary file, file sync
and no-replace publication. Unix files have mode 0600. An occupied destination,
including a dangling symlink or a file appearing during preparation, is never
overwritten. Source/contract/privacy failures do not publish a final session.
Temporary cleanup is best effort. As with bundle export, directory metadata is
not synced and parent-path checks are not a sandbox against hostile directory
replacement. See `model-handoff.md` for the shared durability limitations.

There is no model request, tool execution, live-session mutation, recipient
contact, automatic continuation or automatic import on startup. Import is a
local command in the default terminal; the classic UI is unchanged. The
previously prepared goal keyboard-control integration is not part of this work.

## Test status

Ten new Rust regressions exercise command parsing, the actual workspace
command dispatcher, provider refusal, export/import linkage, native Session
reopening, descriptive-only history, source membership, coverage, JSON bounds,
privacy screening, invalid paths, publication races and Unix permissions/links.
They retain the existing handoff generation/export tests unchanged.

These tests are authored, not executed. On the implementation host,
`dsr quality --tool pi_agent_rust` could not start because `dsr` is absent
(exit 127). Compilation, formatting, Clippy, runtime, PTY and Windows acceptance
remain unverified. No alternate Cargo/RCH gate or GitHub Actions was used.
