# Review and approve exact formatting changes

Document and range formatting use the same retained transaction as reviewed
symbol renames and edit-only code actions. A fresh formatting request previews
by default:

```json
{"action":"format","file":"src/lib.rs","formatOptions":{"tabSize":4,"insertSpaces":true},"timeout":10}
```

The response contains `previewOnly:true`, `applied:false`, `changed`,
`editCount`, byte counts, the effective formatting options, the complete
`workspaceEdit`, and an opaque `refactorId`. No source, scratch file or parent
directory is created or changed by staging. Range formatting accepts an exact
zero-based UTF-16 `range`; the server may expand the edits beyond that range.

Inspect the retained plan without consulting the formatter again:

```json
{"action":"format","refactorId":"<returned ID>"}
```

Approve those exact staged file contents:

```json
{"action":"format","refactorId":"<returned ID>","apply":true}
```

Do not repeat `file`, `range` or `formatOptions` on approval. A selected handle
accepts only its original action, `apply` and `timeout`. Approval consumes the
plan before committing. It does not ask for another formatter response or
recalculate against newer text. The result changes `previewOnly` and `preview`
to false. A null, empty or text-identical formatter result can also be reviewed;
its approval consumes the handle but reports `changed:false` and
`applied:false`, without rewriting the file.

## Complete plans and shortened overviews

The existing `edits` overview is capped at 64 KiB and contains whole edits only.
`previewTruncated` describes that overview, **not** the approval evidence.
`workspaceEditComplete:true` means the complete plan is present separately in
`workspaceEdit`. The entire serialized preview, including both the overview
and the complete plan, must fit within 200 KiB. Otherwise the request fails
with `LSP_EDIT_LIMIT` and does not retain an approval handle. No hidden tail of
an edit can be approved. Direct non-preview application retains the existing
2 MiB response, 16 MiB file and other transaction limits.

## Lifetime, freshness and authority

One reviewed plan is retained per tool instance, shared with rename and code
refactor previews. An admitted fresh format computation replaces that plan,
even if the new provider response fails or exceeds the preview limit. Invalid
selectors, options and local ranges do not consume an existing plan. A handle
expires after five minutes, on reload/server replacement, on a participating
document being closed or resynchronized, or when the retained files no longer
match their original bytes, permissions or existence. Inspection does not
extend the lifetime. Rejected or applied handles cannot be replayed.

Both fresh formatting and approval require the current caller's I/O authority
and cancellation checks. Fresh formatting shares one timeout across server
startup, the formatting request, response validation and staging, starting
after acquisition of the serialized tool workflow lane. The immediate-write
path also checks the same owner, source incarnation and deadline after staging
before entering commit. A failed or cancelled preview never grants a command
window, and unsolicited `workspace/applyEdit` requests remain denied.

Synchronous filesystem I/O is not preemptible. Once commit starts it uses the
existing finish-or-rollback discipline. This is not cross-file atomic
visibility, power-loss recovery, or an OS sandbox against hostile concurrent
path replacement. Incomplete rollback remains an explicit error.

## Immediate formatting remains available

A fresh request with a file and `apply:true` computes and applies immediately:

```json
{"action":"format","file":"src/lib.rs","apply":true,"timeout":10}
```

This is a **new computation**, not approval of an earlier preview. Its response
can differ from that preview, and the old handle is retired. Use `refactorId`
when the intent is to commit exactly what was reviewed.

Protocol fixtures exercise framing and tool sequencing, not language analysis.
Native Rust acceptance is owned by the repository's DSR quality lane.
