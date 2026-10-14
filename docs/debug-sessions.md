# Conversation-owned debugger sessions

The `debug` tool retains one native debugger per current agent conversation.
It uses the same live session-identity binding as jobs and eval; there is no
caller-supplied owner ID or separate frontend selection parameter.

Launch a target in conversation A, set breakpoints and inspect its variables.
After switching to B, `debug {"action":"sessions"}` reports an empty list until
B launches or attaches its own target. B cannot read A's output, continue it,
change its breakpoints, inspect its handles, disconnect it or terminate it.
Switching back to A restores the same adapter, retained breakpoint sets, output
and inspection state. A fork with a new conversation ID starts independently.

Opaque frame/object handles remain bound to their original DAP session and
suspension. A handle returned in A is not usable in B, even when both adapters
use identical native frame IDs. Ordinary resume/invalidation still expires
handles; switching conversations does not make an expired handle valid again.

## Capacity and cleanup

One tool instance retains at most **four concrete debugger sessions**, including
its selected session. Empty conversations and failed launches consume no slot.
At capacity a new launch or attach fails with `DAP_SESSION_CAPACITY` before
spawning another adapter. No least-recently-used target is evicted or terminated
merely to admit a different conversation. Return to the owning conversation and
explicitly disconnect or terminate a target to free its slot.

Switching conversations does **not** pause a running debuggee. Its adapter and
program can keep running and producing output under the existing DAP bounds.
`disconnect` and `terminate` keep their existing attachment/launch semantics;
this feature does not grant a new right to kill an attached process. Dropping
the tool retires all its owned adapters through their existing cleanup paths.
No debugger or process handle is persisted into the conversation log.

## Queued requests and ownership changes

An invocation captures its current nonempty conversation identity before
waiting for the tool's whole-operation lease. It rechecks after admission before
selecting an adapter. An old queued request fails with `DAP_SESSION_CHANGED`
instead of being redirected to the newly selected conversation. Dropping a
queued invocation performs no debugger operation.

The result is checked again before publication. A changed conversation suppresses
that old result, but does not undo an already admitted DAP command or move the
adapter to the new owner. A launch that completes during a switch remains owned
by its originating conversation; return there to inspect or disconnect it.
Unavailable, blank or oversized identities fail with `DAP_SESSION_UNAVAILABLE`
instead of falling back to a previous/global debugger. Identity strings are
bounded to 1024 bytes and other conversation IDs are not included in results.

Each identity lookup has a separate 30-second cooperative budget, including
lookups performed while holding the operation lease. Session-lock contention
remains a wait, not a fabricated missing identity. An unresolved lookup returns
`DAP_SESSION_TIMEOUT`; owner cancellation returns `DAP_CANCELLED`. A 20 ms
watch keeps pending lookups cancellable, and both conditions are checked again
before accepting a ready result. Timeout/cancellation drops that resolver and
releases admission without retargeting or terminating a retained debugger.

An immediately ready resolver needs no timer. A pending resolver requires the
request's timer capability; it is refused with `DAP_PERMISSION` when that
capability is absent. Polling and synchronous resolver cleanup use the original
request context. The wait creates no extra thread and cannot preempt a resolver
that blocks synchronously inside a poll. This is an identity-lookup budget, not
a new whole-debug-command deadline or cancellation of already admitted DAP work.

These admission/publication checks are cooperative, not an atomic frontend
transition lock or an active-request cancellation watchdog. An in-flight DAP
workflow can finish on its originally selected adapter after the user switches.
A complete A-to-B-to-A transition between checks is not observable. Hosts must
still route replies to their originating requests. Completed debuggee effects
are not rolled back, and a newly selected conversation never inherits them.

## Embedding and validation

`ToolRegistry::new`, `from_tools` and later tool mounting use the existing
`bind_job_session_scope` hook. Shared registry snapshots retain the same tool
and resolver. A standalone `DebugTool::new` has its own fixed identity; a host
may bind a live `JobSessionScope`. Moving an owned tool into another registry
retires its previous adapters rather than transferring their ownership.

The primary workspace and adapter configuration stay those of the tool instance;
construct appropriately rooted tools when changing workspaces. This feature is
in-process conversation isolation, not a security sandbox, durable debugger
checkpoint, multiple targets within one conversation, or cross-process sharing.

Regression cases are in `src/debug/ownership/tests.rs`, the identity-wait
module and `tests/debug_sessions.rs`. The latter runs the actual registry,
public tool and DAP transport against the repository's deterministic Python protocol peer, not
a real language debugger. Python must be available; `PI_DEBUG_REQUIRE_PROTOCOL`
makes its absence fail rather than skip. Run the repository's authoritative
`dsr quality --tool pi_agent_rust` workflow for Rust acceptance. Authored source
and protocol tests alone do not establish compilation or a passing quality gate.

## Attach to an existing Python service

Port attachment connects directly to an already-running **debugpy DAP server**.
It does not look up or spawn a local adapter, inject into a PID, install Python
packages, or change the debuggee's working directory. For example, start your
program with its own interpreter and installed debugpy:

```bash
python -m debugpy --listen 127.0.0.1:5678 --wait-for-client --configure-subProcess false app.py
```

Then call the `debug` tool:

```json
{"action":"attach","port":5678}
```

`adapter` may be omitted or set to `"debugpy"`. Port attachment is mutually
exclusive with `pid`, `program`, `args`, `goMode`, and `stopOnEntry`. Existing
PID-based native/Python/Go attachment remains unchanged. This public port path
is not a generic Node inspector, GDB remote, or arbitrary vendor DAP adapter.
It retains the debugger's process/network effects and normal approval policy.

### Forwarded services and source mappings

For a remote/containerized service, establish a trusted tunnel to a local port
separately. Bind the remote debugger safely; do not expose an unauthenticated
debug port to an untrusted network. Debugger access can execute code and alter
the target. Loopback is a routing restriction, **not authentication**.

The default `host` is `127.0.0.1`. Literal loopback IPs are accepted; `localhost`
always means IPv4 loopback without DNS lookup, and `::1` selects IPv6 explicitly.
Other hosts, zero ports and DNS names are refused. Pi does not create the tunnel,
scan ports, authenticate the server, retry connection failures or negotiate TLS.

Map local source to the server's copy and configure breakpoints before allowing
the attached program to run:

```json
{
  "action": "attach",
  "host": "127.0.0.1",
  "port": 5678,
  "pathMappings": [{"localRoot": ".", "remoteRoot": "/srv/app"}],
  "initialBreakpoints": [{"file": "app.py", "line": 42}],
  "justMyCode": true
}
```

At most 16 mappings are accepted. Local roots resolve to existing directories
inside the tool's canonical primary workspace; missing, escaping or duplicate
canonical roots are refused before connecting. Each root is limited to 4096
UTF-8 bytes. `remoteRoot` is passed unchanged to debugpy, using the remote OS's
path conventions. Pi does not fetch remote files, synchronize source copies,
verify that they match, or apply a second path-mapping algorithm. Debugpy maps
breakpoints and returned stack source paths. Keep those source copies aligned.

The existing initialized/breakpoints/configurationDone handshake, exception
filters, suspension-scoped inspection, evaluation and assignments are reused.
Attachment completion does not itself promise a stopped target; use initial
breakpoints, `pause`, and `sessions` to inspect its actual state. `justMyCode`
defaults to true. Child-session debugging and reverse terminal requests remain
unsupported; the attach request sets `subProcess: false`. Configure that on the
external debugpy server before attachment as well when it starts subprocesses.

### Connection lifetime and budgets

The connected transport owns its socket, not the external adapter or process.
It shares the four-conversation capacity and existing ownership checks above.
Successful results report `transport: "tcp"`, `adapterProcessOwned: false`, the
endpoint, resolved mappings and startup budgets. A failed handshake does not
publish a retained session. There is no implicit retry or PID fallback.

`disconnect` requests preservation of an attached target through the existing
DAP policy and closes the client connection. `terminate` remains explicit and
adapter-capability-gated. Transport drop and failed-start cleanup close the
connection without sending an OS signal to an external target. Cancellation of
an already-sent request may retain the connection; it does not undo that request.
Actual target behavior after connection loss is controlled by the server, not
guaranteed by Pi. Completed adapter/debuggee side effects are not rolled back.

`connectTimeoutMs` defaults to 5000, with a range of 1–30000. Initialization has
its existing separate 30-second request budget. `startupTimeoutMs` then covers
attachment and configuration, defaulting to 120000 with a maximum of 300000.
These are separate stages, not a single end-to-end deadline. Connect waits are
cancellable; a bounded OS connect already dispatched may finish later, but an
unclaimed socket closes without sending DAP messages. At most eight connect
workers exist process-wide, including abandoned pending attempts.

Protocol regressions in `tests/debug_attach.rs` exercise the actual tool and
registry without requiring an installed debugger. `src/debug/remote.rs` includes
an explicitly ignored live-debugpy test; opt into it through DSR with a suitable
`PI_DEBUG_TEST_PYTHON` (default `python3`). The independent debugpy wire probe run
during development is not Rust-host or cross-platform acceptance. Both the new
Rust tests and the existing debugger suites still require DSR validation.
