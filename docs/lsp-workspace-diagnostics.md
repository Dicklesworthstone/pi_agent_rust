# Active workspace diagnostics

Use `workspace_diagnostics` to check source files that have not previously been
opened by the LSP tool. The request discovers nonignored regular files under
its working directory, selects each file's configured language server and
workspace root, synchronizes its current text, and obtains its document report.
No separate server startup or file-open request is required.

```json
{"action":"workspace_diagnostics","file":"src/**/*.rs","limit":100,"timeout":30}
```

`diagnostics` retains both its single-document behavior and its server-free,
cached glob view. Active scans are a separate action because they may start
language servers and discover previously unopened files. Scan glob paths
are positive, workspace-relative patterns with forward slashes. `**`, character
classes, and brace alternatives are supported. Absolute globs, parent traversal,
negated globs, control characters, and drive prefixes are rejected rather than
interpreted as empty scans. A leading `./` is permitted.

## Reading the result

The result remains a JSON object, including when individual reports are omitted
for size. `cachedOnly` is false. Each entry has a workspace-relative `file` and
one of these statuses:

| Status | Meaning |
| --- | --- |
| `checked` | A document report was obtained. `count` and `diagnostics` distinguish an explicit empty report from reported findings. |
| `error` | The file could not be checked, the server failed, no report arrived, or the source changed before return. This is not a clean report. |
| `output_limit` | The report did not fit the output budget. Its findings are not silently replaced with an empty array; request that file individually. |

`checkedFiles`, `failedFiles`, `outputTruncated`, and `discoveryErrors` summarize
coverage. A server reporting diagnostics is not itself a tool error. A failure
to obtain a report is a tool error and sets `is_error`; the successful entries
are retained alongside it.

`complete` is true only for a non-continuation request whose entire discovered
scope was checked, without discovery errors, omissions, or limit stops. An
empty matching set is explicit through `matchedFiles: 0`. Check these fields
before interpreting an absence of findings as a clean scan.

Successful source hashes are checked again after processing the other files.
A file changed during that interval becomes an `LSP_DIAGNOSTIC_STALE` error and
its old report is discarded. This does not establish an atomic project
snapshot, freshness of every dependency, or server-side freshness for an
unversioned push report.

## Continuing a large scan

Matching candidates are ordered lexically by workspace-relative path. When
`hasMore` is true and `nextAfter` is present, repeat the same glob with that
literal path as `after`:

```json
{"action":"workspace_diagnostics","file":"src/**/*.rs","limit":100,"timeout":30,"after":"src/model.rs"}
```

The cursor is only a path lower bound, not a server command or filesystem
permission. It must not be URL-decoded. `remainingMatchedFiles` counts discovered
matches after the lower bound; `matchedFiles` counts matches in the whole glob.
`pageComplete` describes the returned page. **Continuation responses always set
`complete` to false**, even on the final page, so a clean last page cannot be
mistaken for a clean whole workspace. Accumulate earlier failures and omitted
reports; do not discard them when advancing.

If traversal was interrupted, exceeded its depth/visit limit, or encountered an
error, `discoveryComplete` is false, `hasMore` is null, and no `nextAfter` is
issued: an unvisited path might sort before that cursor and otherwise be skipped.
Narrow the glob in this case. When a processing budget expires before producing
any entry, a cursor also cannot advance.

There is no persisted snapshot across pages. Files added or renamed before the
current path lower bound require a fresh scan from the beginning.

## Scope and bounds

Discovery honors workspace ignore rules, including `.gitignore` even outside a
Git repository, and excludes hidden entries. Parent-directory ignore rules and
the user's global Git ignore file are not imported. Symlink files/directories
are not followed. The source route is rechecked before reading; this is not an
OS sandbox or protection against every concurrent same-user filesystem race.
Files without configured servers produce explicit errors instead of being
silently treated as checked.

Each request admits up to 100 files by default, with a hard maximum of 256. The
walker visits at most 50,000 entries to a maximum depth of 64. Each source read
is bounded at 8 MiB, with 32 MiB of initial source-text admission per page.
Synchronization and final hash checks read sources again; the admission limit
is not a total I/O or process-memory budget. Existing server/document cache
limits continue to apply.

The scan time budget defaults to 30 seconds and is capped at 120. Each document
report waits for at most five seconds or the remaining scan budget, whichever
is less. The remaining scan budget also bounds asynchronous server startup.
Synchronous filesystem operations and source rechecks cannot be preempted;
this is not a hard wall-clock completion guarantee for blocked filesystem I/O.
Cancellation and the originating owner's filesystem authority are checked
throughout the workflow.

The complete serialized tool result is bounded at 200 KiB. `stopReason` names
file, time, discovery, source, or output limits. Narrow the glob, continue a
proven page boundary, or request omitted/error files individually. None of
these bounded partial results certifies a whole project as error-free.
