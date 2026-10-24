# Review and confirm annotated workspace edits

Language servers can attach labels and descriptions to text edits and file
operations through `WorkspaceEdit.changeAnnotations`. A referenced annotation
with `needsConfirmation:true` must be reviewed before the edit is applied.
Pi retains these annotations rather than discarding them while parsing edits.

## Request a complete preview, then approve that plan

For a symbol rename, request a preview rather than immediate application:

```json
{"action":"rename","file":"src/lib.rs","symbol":"old_name","newName":"new_name","apply":false}
```

The normal `workspaceEdit` and file summaries are accompanied by:

- `changeAnnotations`: the exact definitions referenced by the effective edits,
  keyed by their original identifiers, including labels and descriptions;
- `confirmationRequired`: whether any referenced annotation requires approval;
- `requiredConfirmationIds`: those identifiers in deterministic order.

Inspect the complete edit and its annotations, then approve the retained plan:

```json
{"action":"rename","refactorId":"<returned ID>","apply":true}
```

The same approval mechanism works for `rename_file` import updates and selected
edit-only `code_actions`, including lazily resolved edits. Selecting a listed
`actionId` without `apply:true` resolves and previews an edit-only action.
Its returned `refactorId` is approved using `action:"code_actions"`.

`apply:true` on a valid `refactorId` is explicit approval by the tool caller of
**the entire reviewed transaction**, including all its required annotations.
This is not a new interactive human-confirmation dialog or proof of human
review. There is no partial annotation selection or label-based grouping:
different identifiers remain distinct even when their labels match. A caller
that does not accept the complete change must not approve it.

Successful approval reports `confirmedAnnotations`. It consumes the existing
single-use plan and never repeats the server request or recalculates edits.
Inspecting a plan without applying does not confirm anything or extend its
lifetime. The existing exact-byte, permission, existence, source-incarnation,
connection, expiration and current-caller authority checks still apply.

## Immediate requests and command callbacks

A fresh rename normally applies immediately when `apply` is omitted or true.
An annotation requiring confirmation now stops that application with
`LSP_EDIT_CONFIRMATION_REQUIRED`; request a preview instead. The same boundary
applies to direct code-action edits and the public workspace-edit apply API.
Informational annotations (confirmation omitted or false) do not block ordinary
application. Confirmation does not carry over to a fresh request, even if its
annotation identifier and label match a previously approved edit.

A selected command's `workspace/applyEdit` window is not approval of annotations
that the caller has never seen. A confirmation-required callback is rejected,
and the existing command-window failure handling revokes later callbacks.
Earlier acknowledged batches are not rolled back merely because a later one
needs confirmation; inspect the returned partial-application report. Server
commands may have their own effects, and this feature does not sandbox them.
Command-backed code actions remain unsuitable for a complete static preview.

## Validation and admission

Undefined identifiers, malformed definitions, wrong-typed confirmation flags,
and conflicting outer/inner identifiers in the supported captured `textEdit`
wrapper fail with `LSP_EDIT_MALFORMED`. Standard annotated text edits and
create/rename/delete resource annotations are covered. `documentChanges` keeps
its normal precedence over `changes`; ignored legacy edits cannot authorize or
add confirmation requirements to the effective transaction. Unreferenced table
entries do not require approval, but the entire table must still be well formed.

Admission allows at most 128 definitions, identifiers of 1–128 UTF-8 bytes,
labels of 1–1,024 bytes, descriptions up to 8,192 bytes, and a 64 KiB serialized
annotation table, including escaping and extension metadata. Full previews
retain their existing 200 KiB limit. An oversized complete preview fails without
issuing an approval handle, rather than hiding part of the annotation evidence.

The client negotiates workspace change annotations and reports that rename and
code-action annotations are honored, without claiming grouping by equal labels.
Labels are untrusted descriptive data, not executable commands, paths or grants
of authority. Annotation approval does not expand the workspace boundary or
change the existing rollback, cancellation and cross-file visibility limits.
