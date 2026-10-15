# Eval output archives

Both Python and JavaScript eval can retain console output, rich displays and
the final native cell result in one disk-backed artifact:

```json
{"kernel":"js","output_path":"analysis/run.jsonl","code":"console.log('BEGIN-' + 'x'.repeat(1000000) + '-END'); display({answer:42}); display('# Report','text/markdown'); 42"}
```

`output_path` is optional. Omission keeps the existing bounded-preview behavior
and creates no archive. A supplied value must name a **new, workspace-relative
`.jsonl` file**. Absolute paths, traversal, symbolic ancestors and occupied
destinations are rejected by the shared artifact publisher. The destination is
pinned before execution. Admission errors precede a requested reset, so they do
not erase retained kernel state or run user code. This uses the existing eval
execution path and does not expand the read-only tool bridge whitelist.

## Format and receipt

The current UTF-8 JSON Lines format uses schema `pi.eval.output_artifact.v1`.
A `start` record identifies the schema and kernel. Admitted console frames retain
their `sequence`, `name` (`stdout` or `stderr`) and complete `text`. One `result`
record then contains the native serialized `ToolOutput` in its `output` field,
followed by an `end` record with console sequence/byte counts, `resultIncluded`
and execution status (`success` or `error`). It is data, not an executable file.

The result record preserves native text blocks, Markdown rendering, PNG image
blocks and their base64 bytes, structured JSON in `details.jsonOutputs`, final
expression/error text, cell/reset metadata and other native details. It is the
validated final result, not a potentially sampled running callback. A quiet
expression with no console output still produces a result record. The archive
receipt and the later saved-file notice are not recursively included in it.

This deliberately replaces the console-only `pi.eval.console_artifact.v1`
format for newly written files. Existing files are neither rewritten nor
silently imported. Readers must check the schema and record kind. Concatenate
`text` on stdout/stderr records to recover complete received console streams;
read the single result record for rich output and the final answer. Its native
text also contains the bounded console rendering presented by the tool, so do
not append it to the full console stream and mistake that for fresh output.
The console sequence does not timestamp or order rich displays against stdout.

A completed tool result reports `details.outputArtifact` with `status: written`,
`path`, `format: jsonl`, encoded `bytes`, `sha256`, `complete: true` and
`resultIncluded: true` only after create-only publication succeeds. The digest
covers the bytes actually copied into final staging. It is an integrity value,
not a signature or a trust guarantee. The ordinary read tool and JSONL tooling
can read the file; no new `artifact://` resolver is introduced.

**Complete is not a computation-success claim.** An ordinary code exception can
publish a complete artifact ending with `status: error`, preserving prior plots
and JSON while the tool remains unsuccessful. Final expressions retain each
kernel's existing representation and limits; Python values are not magically
converted from repr into typed JSON. Kernel variables, imports, code execution,
notebook checkpoints and OS effects are not persisted. Direct OS-descriptor
writes and subprocess output keep their existing private-protocol behavior.
Unreceived or in-flight output is not claimed as preserved.

## Bounded storage and publication

JSONL is spooled into a private temporary file with a 64 KiB write buffer.
Console rendering in the prompt still uses bounded head/tail data. The encoded
archive budget is 64 MiB, including escaping, final result and a reserved 4 KiB
end record. Serialization writes through a counting, cancellation-aware sink:
it stops before crossing the budget rather than allocating a second entire
result merely to discover that the result is too large. The final native result
is moved to and returned from the blocking worker, not cloned for archival.
This does not limit allocations already made by guest code or native formatting.

A size or I/O error poisons the private spool permanently. Neither a partial
record nor a successful prefix can receive a complete-file receipt. A failure
while archiving a finished cell preserves that cell's content, structured data
and retained kernel state; it marks the returned tool result unsuccessful and
discloses the archive failure. User code is never rerun to obtain the file.

Publication copies through the existing bounded reader API into synced private
staging and uses create-only publication. Unix uses no-follow directory
descriptors and a hard-link commit; other platforms retain the publisher's
canonical-parent checks and no-clobber persistence. Files are 0600 on Unix.
Temporary spool plus final staging can use roughly twice the encoded file size.
There is no archive retention service or automatic removal of published files.

Preparation and final serialization/publication run on blocking workers outside
the active cell timeout; console spooling during execution counts toward that
timeout. Original owner cancellation is checked during preparation,
serialization, copy chunks and before publication. It cannot interrupt one
blocking syscall or undo a completed publication. Dropping a pending publication
can leave already-admitted work to finish; a missing receipt is not proof that no
file was created. If publication succeeds just before cancellation is observed,
the tool remains unsuccessful with `EVAL_CANCELLED` but retains the actual
written-file receipt. A written receipt is never changed into a no-file claim.

A watchdog interruption has `status: not_published`, `complete: false` and keeps
the existing bounded failure preview instead of publishing an incomplete spool.
Python tool errors keep their error convention and disclose the absent complete
archive. Destination conflicts and publication failures report `status: failed`
and `executionFailed`, preserving completed cell effects and results. Nothing
is automatically retried, rolled back or overwritten.

Console text, JSON, images and native result metadata are **not redacted** in
this explicitly requested local archive. Choose a private destination and a
retention practice appropriate to all of the cell's output, not just what it
prints. This is native output persistence, not complete OMP parity, a notebook
execution/replay service, background eval, or a filesystem sandbox.

Regression coverage lives in `src/eval/output/archive.rs`, its `result_tests`
module, `tests/eval_live_display.rs` and the shared artifact publisher tests.
The implementation environment lacked `dsr` (exit 127); new tests are authored,
not executed. Rust compilation, formatting, Clippy and runtime acceptance remain
unverified. The required quality entry point is:

```sh
dsr quality --tool pi_agent_rust
```
