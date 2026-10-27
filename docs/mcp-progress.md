# Live progress from mounted MCP tools

Mounted MCP tools can report advisory progress through the existing `ToolUpdate`
callback. The callback reaches the same native tool execution event path as
updates from built-in tools; the final MCP result is still the only tool result.
There is no new model tool, permission, automatic retry, or background work queue.

## Wire behavior

When an observer is supplied and the native transport has available progress
capacity, Pi adds a fresh string token to the **request's**
`params._meta.progressToken`. It does not modify `params.arguments`. A caller's
literal `_meta` tool argument is not a progress token and is preserved normally.
Calls without a callback do not request progress. Existing custom transports
can continue to work without implementing the optional progress router.

The stdio reader and both Streamable HTTP SSE receive paths route validated
`notifications/progress` envelopes. Tokens are scoped to one request on one
connection. HTTP routing also uses the receiving stream's session generation;
an old stream cannot publish into a replacement generation's subscription.
If an HTTP session renews during a call, observation of the old generation stops.
Mounted calls and their admitted tool catalogs are bound to their original
logical session; neither calls nor progress are replayed or rebound to a
replacement session automatically.

These are the MCP 2025-06-18 progress utility's request metadata and notification
fields, not a new server capability negotiation:
https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/progress

## Ownership and resource bounds

The server reader only updates a coalescing mailbox and wakes the caller. User
callbacks run on the request's task under its initiating `AgentCx`, outside the
registry locks. Every subscription has one pending status and at most one waker.
There are at most 1,024 subscriptions per transport and 128 delivered updates
per call. Capacity exhaustion disables observation rather than failing a valid
call or evicting another request. The last permitted update explicitly marks
further delivery as suppressed; it still does not claim the tool finished.

Progress must be a finite nonnegative number and strictly increase. Fractional
progress is supported; integer counters retain their integer representation.
Optional totals are nonnegative estimates and may change or be omitted. Unknown
or expired tokens, mismatched generations, malformed progress fields, duplicate
values and regressions are ignored. Existing JSON-RPC envelope validation is
unchanged and can still reject malformed protocol frames.

Messages are quoted, escaped and bounded to 4,096 bytes **after** escaping.
Control characters, bidi overrides and non-ASCII scalars are visibly escaped;
truncation is explicit. Structured update metadata uses the same escaped text.
Progress tokens and message bodies are excluded from the registry/subscription
Debug implementations. This is bounded display handling, not secret redaction.

## Completion, cancellation and deadlines

A queued dispatch lease cannot publish progress before it is claimed. Observation
rechecks owner admission, manager liveness, trust and connection/session identity.
Progress does not extend the existing absolute manager deadline. It does not
change native abort/drain, unsent withdrawal, indeterminate delivery, reconnection
or output-schema validation. No call is retried because progress was missing.

A ready response takes precedence over a queued progress update. Success, server
error, timeout, cancellation, stale ownership, or dropping the enclosing observer
removes its subscription. Late notifications cannot be routed to a later call.
Callbacks are synchronous: blocking callback code is not forcibly preempted and
can delay cooperative cancellation, as with other native tool update callbacks.

## SDK entry point

`McpManager::call_tool` remains the no-observer operation.
`McpManager::call_tool_with_updates(server, tool, arguments, on_update)` accepts
an owned optional native `ToolUpdate` callback. `McpTool::execute` forwards its
existing callback to that operation automatically. Consumers can read
`details.mcpProgress` for `progress`, `total`, escaped `message`,
`messageTruncated` and `deliveryLimited`. This data is informational, not a
completion verdict or permission to execute another operation.

## Remote task status

For a negotiated MCP 2025-11-25 task-augmented tool, the creation receipt is
intermediate. Pi retains the original progress token while it polls the task
and retrieves the final `tasks/result`. Only that underlying tool result
completes the invocation, and normal output-schema validation still applies.
Polling honors the server's interval with a 100 ms minimum; omitted intervals
default to one second. Waiting never extends the original tool-call deadline.

Changed task status and status messages produce advisory `ToolUpdate` values
on the same calling task. `details.mcpTask` contains `status`, escaped
`statusMessage`, `statusMessageTruncated`, and `deliveryLimited`. Status text
uses the same 4,096-byte display bound as progress messages, and at most 128
changed task statuses are delivered per invocation. Task identifiers are not
included in these user-facing updates. A completed status still requires a
valid final result; it does not itself authorize another operation.

Task creation, polling, result retrieval, and cleanup retain the exact
connection generation. Late results cannot replace a renewed session's catalog
or retire that session. Cancellation uses a bounded `tasks/cancel` request when
the server advertises it; it never uses the original creation request ID.
See [task configuration and scope](tools/mcp-configuration.md#task-augmented-tools).

## Validation status

Nineteen Rust tests are authored, **not executed**. They cover correlation,
HTTP generation isolation, malformed and duplicate updates, integer precision,
mailbox and delivery limits, escaping, waiter cleanup, result precedence,
reentrant callbacks, abandoned/unpolled futures, and native mounted-tool
stdio/HTTP workflows. The scripted peer withholds its final response until the
actual tool callback acknowledges live progress; a separate native stdio case
continues sending progress while the manager deadline expires. Additional native
cases cancel the owning call from a live callback and reject an untrusted server
before any progress or server execution. The public-path workflows are Unix-gated
and require Python 3 only as a test fixture dependency. Stdio fixtures bind the
resolved executable identity, matching the normal trust gate rather than relying
on an insufficient fingerprint-only acknowledgement.

The mounted-tool workflow covers stdio, HTTP POST SSE, and an optional HTTP GET
stream carrying notifications while the final result stays on its originating
POST. Each acknowledged phase is scoped to the exact request ID. A separate
unit test rejects notifications from obsolete HTTP receive generations.

An independent probe executed the exact embedded Python peer and passed 23
checks across those three transport modes: handshake/session headers, live
acknowledgements before the final response, unchanged literal tool arguments,
no progress for unobserved calls, and GET/DELETE cleanup. This validates the
fixture's real process/socket behavior, **not** the Rust client, its cancellation
or deadline enforcement, or interoperability with a third-party MCP server.

The required `dsr quality --tool pi_agent_rust` command could not start because
`dsr` was absent (exit 127). Rust compilation, tests, rustfmt and Clippy remain
unverified. Source/patch inspection is not a passing DSR gate. No workflow,
dependency, or quality-gate configuration was changed by that progress patch.

The task extension adds protocol-validation, logical-session isolation, and
native stdio/HTTP fixture regressions. They cover required/optional/forbidden
negotiation, polling and immediate completion, `input_required`, task failures,
original-token progress, malformed and miscorrelated replies, output-schema
validation, session expiry without replay, cancellation, and deadline cleanup.
Additional transport regressions cover legacy SSE task cancellation and queued
requests that lose their logical session before dispatch. These Rust tests are
authored but remain unexecuted until the required DSR quality run is available;
source syntax and patch checks do not establish Rust type or runtime correctness.
