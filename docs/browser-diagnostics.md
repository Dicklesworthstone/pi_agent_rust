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

Unit regressions live in `src/browser/cdp/diagnostics.rs`. Native acceptance
requires the repository's authoritative command:

```text
dsr quality --tool pi_agent_rust
```

The implementation session could not start that command because `dsr` was not
installed. Authored tests are not an executed Rust or live-Chromium result.
