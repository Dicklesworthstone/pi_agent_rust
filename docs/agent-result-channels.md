# Reading sibling results from a native worker

A native worker with an inherited hub channel can use the same `read` URLs as
its parent to inspect that parent's roster and retained worker results:

```json
{"path":"agent://"}
{"path":"agent://scout-1/findings/0/path"}
{"path":"agent://scout-1/report","offset":20,"limit":30}
```

Use the actual run ID, not an agent definition name. Inside a native worker,
`agent://` resolves in the immediate parent's hub, matching the peer IDs exposed
by the existing `hub` roster. Parent-side resolution is explicitly local: a
parent that is itself a child does not forward the request to its grandparent.
A root process without a channel keeps its original local result behavior.

The worker needs the existing `hub` capability in its native launch tool set
so the parent creates its owned channel, and `read` to call the reading tool.
This feature does not grant a disabled tool, start another server, install a
background listener, or expose host filesystem paths as result references.
Invalid, partial, expired or revoked channel configuration fails; it never
falls back to an unrelated local registry with colliding display IDs.

## Waiting for peer replies

After sending a question to a native worker, the parent can wait for its reply
through the same `hub` tool. Use the actual run ID and the sequence returned by
the send:

```json
{"op":"agent","action":"send","name":"scout-1","text":"Which files own authentication?"}
{"op":"agent","action":"wait","name":"scout-1","cursor":17,"waitMs":30000}
```

Here `17` is an example; replace it with the send result's `seq`. A native
worker can use the same operation to await a sibling or `"name":"parent"`.
The authenticated caller determines the inbox being read. `name` identifies
the sender whose replies are wanted, and cannot grant access to another
worker's inbox. A root caller reads only its parent inbox.

The result has schema `pi.agent-hub.wait/v1` and one of these statuses:

| Status | Meaning |
| --- | --- |
| `messages` | New messages from the named peer are available. |
| `peerStopped` | The peer finished, failed, or was killed without a newer retained reply. |
| `historyLost` | Retention evicted messages after the requested cursor; an answer may have been lost. |
| `timedOut` | The requested wait ended with no matching reply observed. The peer may still be working. |

Results include `messages`, `nextCursor`, `hasMore`, `oldestCursor`,
`droppedThroughCursor`, and the peer's known status. Continue a paged read with
`nextCursor` and the **same peer**. A nonzero `droppedThroughCursor` reports a
retention gap even when some newer replies are returned. Omitting `cursor`
starts at the beginning of retained history. Correlation uses sender and
sequence, not an inferred relationship between the message bodies.

`waitMs` defaults to 30 seconds and must be between 1 and 60,000 milliseconds.
The caller's cancellation and tighter inherited deadline still apply. Waiting
never sends or replays a question, consumes a message, starts a model turn, or
revives a finished child. Reading the inbox later still returns those messages.
A terminal peer's retained replies are returned before `peerStopped`.

Native workers use bounded read-only probes on their existing parent channel;
root callers read the local registry. Ordinary lock contention retries inside
the original budget. Authentication, revocation, broken channels and earlier
transport timeouts remain errors. No registry lock is held while waiting, and
no listener or helper thread is added. This is an explicit reply wait; it does
not infer dependency cycles between workers. Child replies arriving after the
last completed probe remain available in the next wait or inbox read.

The regression coverage includes sender filtering, ordered byte-bounded
pagination, actual eviction versus sequence gaps, authenticated inbox ownership,
terminal peers, cancellation/deadline handling, ordinary IPC wait expiry, and
public `HubTool` delivery during the final wait interval. These Rust regressions
were authored but could not be executed in the implementation environment;
the required DSR compiler/test lanes need the configured RCH fleet.

## Result acceptance and privacy

The parent resolves the request through the same native result cache and URL
projection logic as a parent-side read. Successful schema-validated fields
remain typed JSON; missing fields are not null. Running workers have status,
not accepted projected values. Failed, killed, cancelled and schema-invalid
workers cannot supply projections. Bare result reads retain the local
resolver's explicit failure/status markers. No provisional transcript is
promoted into a result. Retention/eviction and namespace-qualified identities
remain unchanged; results are not persisted across restarts.

The existing result secret-obfuscation pass runs in the parent before transport.
The result document reports `metadata.scope: "parentChannel"`. The channel
credential is not returned in metadata, diagnostics, prompts or command-line
arguments. This is the parent's shared result space, not private per-worker
inboxes: transcript access, another worker's inbox, process control and arbitrary
file reads are not new operations on this envelope.

Authentication and live-process ownership are checked before resolution, and
reader identity/cancellation are rechecked before accepting its document. Reads
use the same listener, connection/frame limits and process lifetime as native
peer messaging. Sending messages remains on the unchanged messaging envelope.

## Bounds and execution model

Result documents may occupy up to 2 MiB of encoded JSON, including metadata
and escaping. Small results (up to 128 KiB) use one complete reply. Larger
results stream in UTF-8-safe pages containing at most 16 KiB of content each,
on the same authenticated connection. Metadata is capped at 32 KiB encoded
and appears only in the first page. The reference remains bounded to 4096
bytes; the transport retains its 64 KiB request and 256 KiB per-frame ceilings.
Oversized documents or metadata fail with `PI_HUB_IPC_RESULT_LIMIT`.

One request freezes one already-redacted result document. Pages do not repeat
the result lookup, decoding or redaction, and they do not gain fresh time
budgets. A connection owns its snapshot and releases it on completion,
disconnect, revocation or timeout. There is no global transfer cache or cursor
that another child can reuse. At most the existing eight admitted connections
per child can retain such snapshots; the page buffer is separately bounded.

The receiver checks offsets, total length, reference identity, first-page
metadata and a SHA-256 fingerprint over the complete document. Reordered,
duplicated, corrupt, incomplete, or inconsistently terminated streams fail;
an error never becomes a partial JSON result. The fingerprint checks content
consistency, not reader authorization, which remains the channel's job.
Only a complete, verified virtual document reaches `read` line offset/limit
pagination. Root-process local result retention limits are unchanged.

The existing URL router is synchronous. Its result-channel client therefore
uses nonblocking local TCP with short bounded idle waits, not a nested async
runtime or a detached helper thread. One monotonic budget, at most five seconds
and capped by the caller's inherited deadline, covers connect/write/read.
Cancellation is checked throughout, including before returning the document.
The parent also rechecks the bound reader's live process identity while
publishing a result stream. All pages share the original read budget.
Connection setup is capped at 100 ms and restricted to the validated IPv4
loopback address; no DNS, proxy or remote host is involved. An unsuccessful
request is not automatically replayed. A slow parent can time out; parent-side
local result locking/decoding retains its existing cooperative behavior.

## Validation status

The result-channel tests cover closed envelopes, credential refusal, process
revocation, JSON-escaped size limits, fragmented UTF-8 responses, mismatch and
incomplete-response rejection, and pre-connect cancellation/deadline checks.
Unix native-chain fixtures launch a real Rust test subprocess that invokes
the public `ReadTool` through its inherited parent channel, checks pagination,
and verifies missing-field errors after a previous native worker was accepted.
The larger fixture passes an approximately 160 KiB UTF-8 result through the
same real worker and `ReadTool` path. Additional stream tests cover exact
reconstruction, UTF-8 boundaries, corrupt fingerprints, invalid offsets and
headers, early completion, truncated sockets and reader revocation.
The subprocess helper is intentionally ignored except when launched by that
fixture; no parent-process environment mutation is used.

These Rust regressions are authored, not executed in this environment.
`dsr quality --tool pi_agent_rust` is unavailable (exit 127). Compilation,
formatting, Clippy, native runtime and cross-platform acceptance remain
unverified. JSON and shell syntax checks are not substitutes for DSR.
