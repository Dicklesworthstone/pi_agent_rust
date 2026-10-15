# Native browser pointer sequences

The existing `browser` action `click` supports a native left click (the default),
left double-click, right/context-menu click, middle click and modifier flags.
CSS, snapshot references and semantic selectors share the same execution path;
explicit frame selection retains the existing coordinate projection and policy.

```json
{"action":"click","tab":"editor","selector":"text/Open file","click_count":2}
{"action":"click","tab":"editor","selector":"testid/file-row","button":"right"}
{"action":"click","tab":"editor","selector":"role/button[name=\"Select row\" exact]","modifiers":["Shift"]}
```

`button` is `left`, `middle` or `right`. `click_count` is the integer `1` or `2`;
only left clicks accept `2`. `modifiers` is an array of distinct `Alt`, `Control`,
`Meta` and `Shift` names. Names are case-sensitive. Omitted values mean left,
one click and no modifiers. Explicit nulls, malformed values, duplicate keys in
the modifier array and pointer options on other actions are rejected before
browser launch or connection. Explicit mock mode cannot report these sequences
as real input. Normal host approvals remain in force.

These are native `Input.dispatchMouseEvent` sequences, not DOM `.click()` calls.
A double-click uses two press/release pairs with `clickCount` values `1` then `2`.
A modifier is an event flag, not a sticky key-down that can leak into a later call.
The tool activates the selected tab before computing the clickable point.

## Identity, interruption and outcomes

All steps retain the original document and resolved backend node. The second
click rechecks the same node's visibility, enabled state and hit test; it never
resolves the selector again to a replacement. Navigation, detachment, cancellation,
a protocol failure or loss of the original geometry stops the remaining sequence.
No completed click is replayed. Each admitted press gets one release attempt,
including a press whose acknowledgement failed. Cleanup is best effort under the
same owner, deadline and original page session; there is no cancellation override
or guarantee that a disconnected browser has released its physical button state.

Disabled, inert and `aria-disabled="true"` ancestors are checked through the
composed DOM tree, including open-shadow hosts and assigned slots. Point hit tests
also understand composed descendants instead of mistaking them for an overlay.
This does not grant new access to closed roots, other frames or disallowed domains.

Successful results report `dispatch_acknowledged:true` and
`delivery_verified:false`. This distinction is intentional: a CDP acknowledgement
is not proof that page handlers ran, a request succeeded, or a file was opened.
Inspect the page or request a post-action snapshot to verify application state.
Failures report the number of fully acknowledged pairs and warn that earlier
side effects may already have occurred. Do not automatically repeat a failed
sequence. Right/middle/modifier clicks can open menus or new tabs depending on the
application; they do not automatically select or operate those new surfaces.

## Verification

`src/browser/interaction/pointer.rs` tests parsing and native event fields.
`tests/browser_pointer.rs` checks public preflight and explicit mock refusal, plus
opt-in real-Chromium workflows for native event types, modifiers, replacement
interruption, disabled/inert targets and frame-scoped double-clicks. An independent
Chromium probe executes the exact DOM helper and CDP event format; it is not a
Rust-host or cross-platform test. Native acceptance requires
`dsr quality --tool pi_agent_rust`; no Cargo/Actions substitute is claimed.
