# Model-assisted terminal handoff

The default terminal can synthesize a continuation brief with its currently
selected model:

```text
/handoff --model
/handoff --model -- focus on remaining verification and the unanswered deployment question
```

This is an explicit, one-shot auxiliary model request. Bare `/handoff` and the
existing human/agent/Bead delivery forms retain their provider-free behavior.
The model form only displays a preview. It does not write files, send mail or
Bead comments, create/switch sessions, change a goal, or resume execution.
Recipient and output-path arguments are intentionally not accepted by this form.

## What the successor receives

The preview contains the goal, current state, decisions, failed approaches,
open questions/blockers, and recommended next steps. Every generated item must
reference one or more supplied evidence IDs. IDs are positions on the selected
session branch; the source map labels user requests, assistant claims, tool
results, Bash results, and compaction summaries separately.

Source-ID validation proves only that the referenced records were supplied.
It is **not** a semantic proof that the model's claim follows from those records.
Assistant claims and summaries are not independent test or filesystem evidence.
Review the preview and actual tool results before a successor acts.

Only the selected branch is read. Other branches and abandoned attempts are not
merged into the successor's task. The first user request and newest compaction
summary are considered as anchors, followed by recent complete records. Up to
256 recent candidates are examined, at most 64 records are sent, each record
must fit 16 KiB encoded, and total evidence fits 48 KiB. Oversized records are
omitted whole, not sliced into misleading prefixes. The preview reports its
coverage and omission count, including non-conversational entries.

Image/audio/video bytes, thinking blocks and provider thought signatures are
not sent as text. The preview cannot claim visual understanding of an omitted
attachment. Session traversal itself uses the existing branch implementation;
these payload limits are not a new preemptible sandbox for a corrupt session.

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
The response and selected source are screened together before display so echoed
source secrets do not become a raw preview. Block-mode refusal remains refusal.
The omitted parts of a large transcript are not rescanned by this command.

A local JSON Schema requires the complete output shape, bounded sections and
valid source IDs. Duplicate keys, markdown wrappers, extra fields, unknown source
IDs, missing terminal events and failed provider responses are rejected. There
is no repair request, heuristic fallback or partial successful brief. After the
response, the selected branch is read again; changed identity, leaf or selected
evidence produces `HANDOFF_STALE` instead of presenting an old preview as current.
This read is a consistency check, not an exclusive lock preventing later edits.

The ordinary terminal UI's keyboard-cancellation integration for auxiliary
workspace commands is unchanged by this feature. Owner cancellation and the
bounded auxiliary deadline remain authoritative; no new background task or
keyboard-control claim is introduced.

## Validation

The implementation includes controlled-provider tests through the real terminal
dispatcher and native Agent/Session types. They cover provider-free legacy behavior,
model/credential selection, isolated request state, source references, unchanged
history, branch-only bounded evidence, nontext omission, secret screening, invalid
arguments, schema/provider failures and stale-source refusal.

These Rust tests are authored, not executed on the implementation host. The
required acceptance entry point is `dsr quality --tool pi_agent_rust`; the host
has no `dsr` executable. No compilation, Clippy, formatting, live-provider, PTY
or cross-platform acceptance is claimed.
