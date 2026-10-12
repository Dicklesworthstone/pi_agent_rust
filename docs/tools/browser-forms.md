# Structured browser form controls

The native browser tool supports `select_option` for ordinary HTML select controls.
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
and their microtasks can run. A replaced/detached node, changed option identities
or metadata, or a reverted selection is an error, not success. No mutation is
retried. Failure/cancellation cannot roll back handlers or page/network effects
that already occurred, and the browser may change again after verification.
This is not an atomic browser transaction.

Bounds: at most 100 requested/returned selected options, 4,096 DOM options,
64 KiB of requested or returned option text, and 256 KiB of inspected option
metadata. Text budgets count UTF-8 bytes. Unknown fields and implicit dialog
consent are rejected before connection or launch. Mock mode refuses the action.

Validation: the initial JavaScript helper passed 18 independent Chromium 144
protocol/helper probes, including idempotency, multi-select, atomic rejection,
disabled/inert controls, handler interference, literal strings and byte limits.
That probe used an isolated test browser with `--no-sandbox` because the probe
container runs as root; the production launcher is unchanged. It did not execute
the Rust adapter. Rust unit/preflight and an ignored live-browser scenario are
included. The authoritative command remains `dsr quality --tool pi_agent_rust`;
no DSR, compilation, Clippy or Rust test pass is claimed by the helper probe.
