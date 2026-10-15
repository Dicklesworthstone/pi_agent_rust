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

## Acceptance and privacy

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

The first implementation accepts complete result documents up to 128 KiB of
encoded JSON, including metadata and escaping. Larger documents fail with
`PI_HUB_IPC_RESULT_LIMIT`; request a smaller structured field. It never returns
a silently truncated JSON value. The existing `read` line offset and limit
apply after the complete virtual document arrives. The reference is bounded to
4096 bytes; the inherited transport retains its 64 KiB request and 256 KiB
response-frame ceilings.

The existing URL router is synchronous. Its result-channel client therefore
uses nonblocking local TCP with short bounded idle waits, not a nested async
runtime or a detached helper thread. One monotonic budget, at most five seconds
and capped by the caller's inherited deadline, covers connect/write/read.
Cancellation is checked throughout, including before returning the document.
Connection setup is capped at 100 ms and restricted to the validated IPv4
loopback address; no DNS, proxy or remote host is involved. An unsuccessful
request is not automatically replayed. A slow parent can time out; parent-side
local result locking/decoding retains its existing cooperative behavior.

## Validation status

The result-channel tests cover closed envelopes, credential refusal, process
revocation, JSON-escaped size limits, fragmented UTF-8 responses, mismatch and
incomplete-response rejection, and pre-connect cancellation/deadline checks.
A Unix native-chain fixture launches a real Rust test subprocess that invokes
the public `ReadTool` through its inherited parent channel, checks pagination,
and verifies missing-field errors after a previous native worker was accepted.
The subprocess helper is intentionally ignored except when launched by that
fixture; no parent-process environment mutation is used.

These Rust regressions are authored, not executed in this environment.
`dsr quality --tool pi_agent_rust` is unavailable (exit 127). Compilation,
formatting, Clippy, native runtime and cross-platform acceptance remain
unverified. JSON and shell syntax checks are not substitutes for DSR.
