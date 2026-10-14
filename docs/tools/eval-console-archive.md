# Eval console archives

Both Python and JavaScript eval can keep a complete disk-backed transcript of
received console frames rather than losing the middle of a large result:

```json
{"kernel":"js","output_path":"analysis/run.jsonl","code":"console.log('BEGIN-' + 'x'.repeat(1000000) + '-END');"}
```

`output_path` is optional. Omission keeps the existing bounded-preview behavior
and creates no console archive. A supplied value must be a string naming a **new,
workspace-relative `.jsonl` file**. Absolute paths, traversal, symbolic ancestors
and occupied destinations are rejected by the shared artifact publisher. The
resolved destination is pinned before execution. Admission errors precede a
requested reset, so they do not erase retained kernel state or run user code.
This option does not add a new tool or expand the eval tool bridge whitelist.

## Format and receipt

An archive is UTF-8 JSON Lines, not an executable script or a plain concatenation
of streams. It has a `start` record with schema `pi.eval.console_artifact.v1` and
kernel name, then each admitted console frame's `sequence`, `name` (`stdout` or
`stderr`) and complete `text`, and an `end` record with the final sequence, byte
counts and execution status (`success` or `error`). Concatenate `text` by stream
to recover its content. Sequence records host admission order, not timestamps.

A completed tool result reports `details.outputArtifact` with `status: written`,
`path`, `format: jsonl`, encoded `bytes`, `sha256`, and `complete: true` only after
create-only publication succeeds. The digest describes bytes read into the final
staging file. It is an integrity value, not a signature or a trust guarantee.
The path can be read with the existing local read tool or external JSONL tooling.
There is no new `artifact://` resolver in this feature.

**Complete refers to received console frames, not successful computation.** An
ordinary code exception can publish an archive ending with `status: error` while
the tool remains unsuccessful. Existing rich displays, final-expression values,
tracebacks and session metadata remain in their established result fields; they
are not extra archive records. Python direct OS-descriptor writes and inherited
subprocess output retain the existing private-protocol behavior, not automatic
capture. In-flight or unreceived output is not claimed as preserved.

## Bounded storage and publication

Captured JSONL is spooled into a private temporary file with a 64 KiB write
buffer. Session results still use the existing bounded head/tail rendering;
archive size does not turn into an equally large prompt or in-memory transcript.
The encoded archive budget is 64 MiB, including JSON escaping and a reserved
4 KiB allowance for its end record. Exceeding the budget or a spool I/O failure
stops the call with an explicit artifact diagnostic rather than silently dropping
bytes. A failed spool can never be published as a successful prefix.

Final publication copies through the shared bounded reader API into private,
synced staging and uses create-only publication. Unix uses the existing
no-follow directory descriptors and hard-link commit; other platforms retain the
publisher's existing canonical-parent checks and no-clobber persistence. The
final file is private (0600 on Unix). Temporary spool plus final staging can use
roughly twice the encoded archive size while publishing. There is no archive
retention service or automatic removal of the published file.

Preparation and final publication run on blocking workers outside the active
cell timeout; spooling during execution counts toward that timeout. Original
owner cancellation is checked during preparation, between copy chunks and before
publication. It cannot interrupt a single blocking syscall or undo a publication
that already happened. Dropping a pending publication can leave already-admitted
work to finish; a missing caller receipt is not proof that no file was created.
If publication succeeds just before owner cancellation is observed, the tool
remains unsuccessful with `EVAL_CANCELLED`, but retains the actual written-file
receipt. A written receipt is never changed into a false no-file claim.

A watchdog interruption has `status: not_published`, `complete: false` and keeps
the existing bounded failure preview instead of publishing an incomplete spool.
Python tool errors retain their error convention and explicitly disclose that
no complete archive was published. A final destination conflict or publication
failure changes an otherwise completed call into an unsuccessful tool result,
records `status: failed` and `executionFailed`, and preserves the completed
cell's state and result. It never retries user code or overwrites the competing
file. Successfully published files and completed code effects are not rolled back.

Console text is **not redacted** in this explicitly requested local archive.
Choose a destination and retention practice appropriate for the data printed by
the cell. This is console archival, not full OMP artifact parity, rich-display
archival, managed background execution, or a filesystem sandbox for arbitrary
Python code.

Regression tests are in `src/eval/output/archive.rs` and `src/artifact_output.rs`.
The authoritative quality entry point remains `dsr quality --tool pi_agent_rust`.
