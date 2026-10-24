# Browser diagnostics around real actions

Native `browser` calls can capture console output and uncaught exception events
on the same CDP connection that performs the requested action. This helps debug
an application instead of treating a successful input acknowledgement as proof
that its JavaScript worked. It continues the OMP browser diagnostics work through
Pi's existing browser tool, not through a second browser runner or Eval bridge.

```json
{
  "action": "goto",
  "tab": "app",
  "url": "http://localhost:3000/",
  "diagnostics": {"settle_ms": 500, "max_events": 100},
  "timeout_ms": 30000
}
```

Capture starts before navigation, evaluation, or input. A fixed receive window
follows a successful action. It shares the existing whole-operation deadline;
console activity never refreshes the deadline or triggers a retry. `settle_ms`
accepts 0 through 10000, defaults to 250, and is not an application-ready or
network-idle test. Zero captures only the events received before the action ends.

To observe an existing page without triggering another action:

```json
{"action":"diagnostics","tab":"app","diagnostics":{"settle_ms":1000}}
```

The standalone action accepts only `action`, `tab`, `diagnostics`, and
`timeout_ms`. It never evaluates a supplied script, clicks, or navigates.
Alternatively, attach `diagnostics` to `evaluate`, `click`, `type`, `fill`,
`press`, `scroll`, `wait_for`, `snapshot`, `ax_tree`, `select_option`, or
`set_checked`. An explicit tab is always required. Frame-scoped calls,
lifecycle commands, `handle_dialog`, uploads, downloads, and exports reject
this option before connecting. Existing calls without the option are unchanged.
The explicitly simulated backend cannot manufacture these diagnostics.

## Network failures and redirects

Add `"network": true` inside `diagnostics` to also observe request lifecycles:

```json
{
  "action": "click",
  "tab": "app",
  "selector": "#save",
  "diagnostics": {"network": true, "settle_ms": 1000},
  "timeout_ms": 30000
}
```

`Network.enable` runs on the same attached connection before the click. It does
not intercept requests, disable caching, change cookies, fetch response bodies,
or retry failures. This option defaults to false; ordinary console capture does
not enable Network or retain unsolicited network events. Console capture remains
enabled when network capture is requested.

`details.diagnostics.network.requests` contains a bounded tail of request hops
in observed start order. Each has a local `sequence`, sanitized URL, method,
resource type, start timestamp and state. Responses add HTTP status, MIME type,
protocol and cache/service-worker flags when supplied. Loading completion or
failure adds elapsed milliseconds calculated from the browser's monotonic
timestamps, optional encoded byte count or a bounded Chromium `net::ERR_*` code,
blocked/CORS reason and cancellation flag. Unknown fields remain absent or null;
no response size, time, status or successful completion is invented.

**A completed HTTP 503 is not the same as a failed connection.** The former has
`state: "completed"` and a response status; the latter has `state: "failed"`
and, when supplied, an error code such as `net::ERR_CONNECTION_REFUSED`. These
application/network failures are diagnostic data and do not change a successful
click into a transport-level tool failure. Inspect the request records as well
as the action result.

Redirects reuse CDP request IDs but produce separate records linked with
`redirect_from`. Each successor is independently checked against the main-frame
and domain policy. A forbidden successor is omitted rather than inheriting the
old hop's admission. A first-seen redirect reports `redirect_start_missing` when
the preceding hop was not observed. Responses without an admitted start, wrong
frame/URL responses, backward timestamps and ambiguous repeated starts cannot
fabricate a new successful request. Invalid and filtered events are counted.

Only admitted main-frame requests are retained. Child-frame and worker traffic,
non-web URLs and requests whose starts predate this connection are not a complete
network history. `complete_history` is always false. This is not a HAR export,
body inspector, profiler, persistent request log, or network-idle detector.

Network retention has its own `max_events`/64-KiB latest-hop budget and a maximum
of 1024 remembered request identities per call. Eviction does not permit an old
identity to be rebound. At the identity ceiling new IDs are refused and reported
as `admission_stopped`, without interfering with the action. Pending or merely
responded requests stay pending at window end; `pending_at_end` counts those still
retained, not all browser traffic. `truncated`, dropped counts and field-shortening
flags make incomplete output visible. URL path shortening is flagged after URL
normalization and query/fragment removal.

Request/response headers, cookies, post data, bodies, extra-info events, initiator
objects and arbitrary network error prose are never copied into this report.
URL queries/fragments are removed, not hashed. URL paths and console text can
still contain application secrets; the existing opt-in warning applies.

## Results and failures

`details.diagnostics` is structured `pi.browser.diagnostics.v1` data. The same
bounded report appears in model-visible text. The original action's content
and details remain present. Console records contain primitive arguments,
source locations, and receive phases (`setup`, `action`, `settle`). Exception
records contain a bounded message/description and source locations. Stack line
and column numbers retain CDP's zero-based numbering. Timestamps, when present,
are browser-provided; no host timestamp or causal ordering is invented.

Chromium may deliver buffered console entries when Runtime is enabled. Reports
explicitly set `console_may_include_buffered: true`: capture is not proof that
every recorded message was caused by this call. Nothing clears the browser's
console. Object arguments remain opaque; Pi never requests properties, invokes
getters, or copies object previews, DOM nodes, or remote-object IDs.

A failed native action remains a tool error, with any already-captured records.
A receive error, event flood, or lost main-document scope also reports an error;
`action_succeeded` separately says whether the original action had completed
successfully before that capture failure. `action_started: false` distinguishes
setup failure. Inspect `completion` and do not automatically replay a mutation:
its effects may already have happened. Outer timeout/cancellation can still
return the ordinary error without a partial report. No diagnostic option extends
that deadline or rolls back completed work.

## Scope, limits, and privacy

Only the exact attached page session and its admitted main-frame default
execution contexts contribute console records. Child frames, isolated worlds,
other tabs, workers, missing contexts, and destroyed contexts do not. Context
origins must match the admitted document. Observed forbidden navigation, root
frame replacement, or detachment stops capture rather than adopting another
page. This is cooperative observation, not an atomic navigation lock or browser
sandbox. The existing domain allowlist still governs admission; diagnostics do
not implement network interception.

`max_events` accepts 1 through 200 and defaults to 100. Console retention also
has a 64-KiB serialized-record budget. The oldest records are evicted first so
recent failures survive earlier console noise. Dropped and filtered records are
counted. Each record takes at most eight arguments and eight stack frames; text
fields are bounded in UTF-8 bytes and control characters are replaced. Reports
identify both tail loss and field shortening. Context tracking is bounded to
64 entries, and post-action draining has the existing 8192-frame event limit.

Source URLs exclude query strings and fragments; credential-bearing and
non-web source URLs are omitted. **Free-form console messages and exception
text may still contain application secrets.** Capture is opt-in, not a general
secret scrubber. Avoid logging credentials in the application. All captured
text remains untrusted application data, never tool instructions or approval.

Capture state belongs to one operation. Socket closure retires its inspector
handles and subscriptions. There is no background reader, cross-call console
store, persistent subscription, disk archive, or automatic action replay.

Unit regressions live in `src/browser/cdp/diagnostics.rs` and its `network` module.
`tests/browser_diagnostics.rs` exercises the public tool against a bounded
HTTP/WebSocket protocol peer: capture-before-action ordering, success/failure,
passive observation, setup refusal, scope loss, network opt-out and post-action
events. It is a protocol fixture, not a live Chromium test. Native acceptance
requires the repository's authoritative command:

```text
dsr quality --tool pi_agent_rust
```

The implementation session could not start that command because `dsr` was not
installed. Authored tests are not an executed Rust or live-Chromium result.
