# Structured browser form controls

The native browser tool supports `select_option` for ordinary HTML select controls
and `set_checked` for ordinary checkbox and radio inputs.
It uses the same tab selection, optional frame scope, URL policy, deadline,
cancellation and backend-node references as other browser interactions.

```json
{"action":"select_option","tab":"work","selector":"#country","options":[{"value":"CA"}]}
{"action":"select_option","tab":"work","selector":"@e4","options":[{"label":"Canada"}]}
{"action":"select_option","tab":"work","frame":"FRAME_ID","selector":"#roles","options":[{"index":0},{"value":"reviewer"}]}
{"action":"select_option","tab":"work","selector":"#roles","options":[]}
```

Each option selector has exactly one `value`, `label`, or zero-based `index`.
Value and label matches use exact DOM strings, with no substring matching,
case folding, or trimming by Pi. A missing or ambiguous match is an error;
use an index to distinguish duplicate values/labels. Duplicate selections are
rejected. More than one option requires a `multiple` select. An empty array
explicitly clears the selection, including a single-select control.

All requested options are resolved and validated before selectedness changes.
Disabled controls (including disabled fieldsets), disabled options/optgroups,
inert ancestors and non-select elements are refused. The selector is resolved
once; subsequent work and verification use that exact backend node and document.
Same-process and cross-process frames use their owning isolated DOM worlds.
Frame selection still requires an explicit `tab`.

Selection uses native DOM property setters, then dispatches `input` and `change`
only when state changed. These events are **not trusted native mouse/keyboard
events**; applications requiring a physical interaction may reject them. The
result states `input_method: "dom-selection"`, `changed`, `multiple`, and the
selected options in document order (index, value and label). Already-selected
requests are no-ops: they do not repeat page event handlers.

Pi verifies selection again in a separate protocol command after event handlers
and their microtasks can run. A replaced/detached control or a reverted selection
is an error, not success. Synchronous handlers changing option identities or
metadata are rejected; the later verification compares selected indices, values
and labels, not the identity of every option node. No mutation is retried.
Failure/cancellation cannot roll back handlers or page/network effects
that already occurred, and the browser may change again after verification.
This is not an atomic browser transaction.

Bounds: at most 100 requested/returned selected options, 4,096 DOM options,
64 KiB of requested or returned option text, and 256 KiB of inspected option
metadata. Text budgets count UTF-8 bytes. Unknown fields and implicit dialog
consent are rejected before connection or launch. Mock mode refuses the action.

## Set checkbox or radio state

```json
{"action":"set_checked","tab":"work","selector":"#subscribe","checked":true}
{"action":"set_checked","tab":"work","selector":"@e9","checked":false}
{"action":"set_checked","tab":"work","frame":"FRAME_ID","selector":"#delivery-express","checked":true}
```

`checked` is a required boolean, never a toggle command. Pi reads the current
state of the pinned control first. An already-matching state is a no-op with
`changed: false`; it does not activate the page, scroll, or fire a click. Otherwise
Pi activates the page, scrolls the same node into view, checks native hit-test
geometry (including cross-process iframe projection), rechecks state, and issues
one native mouse press/release pair. A page-induced state change during preparation
can also turn the operation into a no-op. Selection of a radio uses the browser's
native group behavior, rather than manually changing unrelated controls.

These clicks are native CDP pointer input, not `element.click()` or assignments
to `checked`. Results report `input_method: "native-pointer"`, `kind`, `checked`
and `changed`. Verification uses the original node and document in a subsequent
command; `preventDefault`, handler/microtask reversion, a type change, detachment
or a navigation cannot become a reported success. A failed click is not retried.
One best-effort mouse release is attempted on the original page even if the press
acknowledgement fails, under the same cancellation/deadline; cleanup is not
guaranteed after disconnection or cancellation.

Setting an already-unchecked radio to false is a no-op. Changing a checked radio
to false is refused: select another radio in its group instead. Indeterminate
inputs, custom ARIA widgets, disabled/inert controls and non-checkable elements
are refused. Controls that need a click must also be visible and unobstructed.
The operation cannot undo event handlers, page navigation or other effects that
occurred before a later error. No network or filesystem sandbox is implied.

Validation: the initial JavaScript helper passed 18 independent Chromium 144
protocol/helper probes, including idempotency, multi-select, atomic rejection,
disabled/inert controls, handler interference, literal strings and byte limits.
That probe used an isolated test browser with `--no-sandbox` because the probe
container runs as root; the production launcher is unchanged. It did not execute
the Rust adapter. Rust unit/preflight and an ignored live-browser scenario are
included. The authoritative command remains `dsr quality --tool pi_agent_rust`;
no DSR, compilation, Clippy or Rust test pass is claimed by the helper probe.

The extended helper passed the same 18 selection cases and 10 additional
Chromium helper/native-input probes for trusted check/uncheck, radio groups,
canceled/reverted events, mixed state, disabled/inert controls and stale handles.
Public HTTP/WebSocket protocol-peer regressions cover dispatcher reachability,
single-node resolution, retained-state verification, exact native input counts,
release on failed press, and no click after a preparation-time state change.
The protocol peers and the ignored live Rust scenarios were authored, not run;
the independent probes do not execute the Rust adapter or establish DSR success.
