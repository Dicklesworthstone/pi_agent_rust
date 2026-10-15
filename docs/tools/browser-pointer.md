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
the modifier array and click-specific options on other actions are rejected before
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

## Native hover and drag-and-drop

`hover` and `drag` are native pointer workflows on an **explicit main-document
tab**. They use the same CSS, semantic and snapshot-reference resolver as clicks.
They do not evaluate a supplied script, manufacture a `DragEvent`, extract drag
data into the model context, or inject an invented `DataTransfer` payload.

Open a hover-driven menu and wait for its rendered state:

```json
{
  "action": "hover",
  "tab": "board",
  "selector": "role/button[name=\"Show menu\" exact]",
  "wait_after": { "selector": "#menu", "text": "Menu ready" },
  "timeout_ms": 5000
}
```

Hover sends one unpressed native pointer movement. It does not click or request
keyboard focus, although the page's own pointer handlers can have effects.

Move a card with the page's ordinary HTML5 drag-and-drop behavior:

```json
{
  "action": "drag",
  "tab": "board",
  "selector": "text/Move card",
  "target_selector": "testid/drop-zone",
  "drag_steps": 12,
  "duration_ms": 300,
  "wait_after": {
    "selector": "#status",
    "text": "Moved successfully",
    "text_match": "exact"
  },
  "timeout_ms": 10000
}
```

The native sequence moves to the source, presses the left button once, sends
interpolated held-pointer movements, repeats the destination movement to permit
`dragover`, and releases once. HTML drag data comes from the page/browser, not
Pi. Custom pointer/mouse drag handlers, including pointer capture, can receive
the same native sequence. This does not force a target to accept a drop.

Both actions accept the existing distinct `modifiers` names. Flags are sent on
every native input frame; browser-generated completion events may have different
flags. In particular, the independent Chromium probe observed Shift on native
drag initiation and drop but not on the generated final `dragend` event.
`button` and `click_count` remain click-only. `drag_steps` is an integer from 1
through 64 (default 12), deliberately distinct from batch `steps`.
`duration_ms` is 0 through 5000 (default 300), the requested movement pacing
span rather than a new deadline. Protocol and ownership checks can take longer.

### Original-node and viewport discipline

The source and destination are resolved exactly once. Source scrolling happens
before pointer input. Both elements must then have usable hit points in the same
viewport; they must be different nodes with points at least eight CSS pixels
apart. The minimum movement avoids a deliberately stationary click, but does
not guarantee that a platform or application recognizes a drag.

Hover-triggered source movement is checked before pressing. Once held, the
source may move or hide as part of a native drag, but must remain the original
connected, enabled node. The destination's clipped bounds must remain within
half a CSS pixel of their original positions; the final drop also requires its
original hit point to remain unobstructed. Replacement, navigation or observed
target movement stops the sequence instead of choosing new coordinates or nodes.
Checks are cooperative observations, not a lock on the page's layout.

This implementation does **not** support frame-scoped gestures, cross-frame or
cross-tab drags, automatic scrolling while held, same-element slider-coordinate
drags, or external file dragging. `frame`, dialog-consent fields, diagnostics,
foreign input options and malformed gesture fields are rejected before launch.
Use the existing upload tool action for intentional file-input selection.

### Interruption, postconditions and batches

On an observed error after a press was attempted, Pi attempts native
`Input.cancelDragging`, then one release at the last attempted coordinate on the
original page. A failed press acknowledgement does not prove the press had no
effect. A failed final release acknowledgement does **not** trigger another
release or press. Custom page handlers can still react to cleanup.

These attempts share the original owner and deadline. Cancellation, deadline
expiry, a dropped future or connection loss can prevent cleanup; the tool does
not claim that the browser's pointer state was reset in those cases. Completed
effects are not undone. There is no implicit retry or input replay.

Observed drag failures after input return an unsuccessful tool result retaining
`gesture` progress: acknowledged movement count and press/release/cancel
attempted/acknowledged flags. They do not echo arbitrary page exception values.
Successful results still report `delivery_verified:false`: native acknowledgement
alone does not establish application acceptance. An application rejecting the
drop can therefore produce an acknowledged gesture with no application change.

`wait_after` can verify a same-document state/text/stability condition after the
gesture, on the same connection and original deadline. Navigation postconditions
are not supported for gestures. An observed postcondition failure retains the
completed gesture's progress and marks the result unsuccessful. Both actions
are admitted inside native batches; gesture or postcondition failure stops later
steps and retains completed evidence, without replaying the batch.

## Verification

`src/browser/interaction/pointer.rs` tests parsing and native event fields.
`tests/browser_pointer.rs` checks public preflight and explicit mock refusal, plus
opt-in real-Chromium workflows for native event types, modifiers, replacement
interruption, disabled/inert targets and frame-scoped double-clicks. An independent
Chromium probe executes the exact DOM helper and CDP event format; it is not a
Rust-host or cross-platform test. Native acceptance requires
`dsr quality --tool pi_agent_rust`; no Cargo/Actions substitute is claimed.

Gesture regressions extend these same test files rather than a second browser
implementation. `tests/browser_wait.rs` uses its independent HTTP/WebSocket
peer to exercise the public dispatcher, native event sequence, failed press/move/
release acknowledgements, replacement and geometry drift, cancellation, batch
stop and postcondition integration. `tests/browser_pointer.rs` adds public
preflight/schema/mock checks and explicitly opt-in live HTML drag/hover and
source-replacement workflows. The new Rust tests were authored but not executed
on the implementation host because DSR was unavailable. Independent Chromium
experiments exercise the exact embedded DOM helper and native input format;
they do not establish Rust-host, cross-platform or release acceptance.
