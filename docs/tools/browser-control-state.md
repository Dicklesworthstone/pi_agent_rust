# Browser control-state observations

Native `snapshot` and `ax_tree` now retain a bounded, typed selection of
Chromium's accessibility properties, rather than only names and roles.
A snapshot line can read:

```text
- [@e7] <input> (role: checkbox) "Approve" [checked="mixed", focusable=true]
- [@e8] <button> (role: button) "Menu" [expanded=false, hasPopup="menu"]
```

The existing snapshot element structure and reference semantics are unchanged.
`details.element_states` maps each returned snapshot reference to its observation:

```json
{
  "@e7": {"states": {"checked": "mixed", "focusable": true}, "incomplete": false},
  "@e8": {"states": {"expanded": false, "hasPopup": "menu"}, "incomplete": false}
}
```

`details.state_schema` is `pi.browser.ax-state.v1`. Full `ax_tree` nodes also
carry `state_observation` in the same shape, including nodes not in the compact
200-element inventory. Its existing independent 1000-node limit still applies.
The native frame-selection and conversation ownership rules remain in force.
There are no extra browser calls, JavaScript evaluations, input actions or
persistent subscriptions to obtain these states: they come from the same
accessibility response used for that snapshot.

## Supported states

Booleans: `busy`, `disabled`, `focusable`, `focused`, `settable`, `atomic`,
`multiselectable`, `multiline`, `readonly`, `required`, `expanded`, `modal`,
`selected`. `checked` and `pressed` are booleans or the literal string `mixed`.

Known tokens are retained for `editable`, `invalid`, `live`, `autocomplete`,
`hasPopup` and `orientation`; integer `level` is bounded to 1–1000000.
`invalid` and `hasPopup` retain their protocol tokens, including the strings
`"true"` and `"false"`; they are not JSON booleans. The captured Chromium
`busy` wire representation also permits exact integer 0/1 under AX type
`boolean`. This is not general numeric or string truthiness.

Only explicitly supplied known states appear. A missing `disabled`, for
example, is unknown/not reported, NOT proof that the control is enabled.
An undefined state is omitted. Duplicate recognized state names are removed
rather than choosing a winner. Invalid recognized types/values mark the
observation `incomplete: true`, as does a malformed or over-64-entry property
list. Unknown properties are deliberately outside this projection; an
`incomplete: false` observation does not promise an exhaustive accessibility
model or independent verification of a page's ARIA claims.

The textual annotation and structured state map carry the same values. This
allows existing batch assertions to inspect an already-returned state using
pointers such as `/element_states/@e7/states/checked`. Obtain references from
that page/frame's snapshot; examples do not establish a real @e7 in a session.
Use an additional assertion on `incomplete` when complete supported-state
projection is required. Assertions do not authorize another action or make
snapshot-time state immutable.

## Disclosure and limits

This projection never copies editable values, `valuetext`, descriptions,
relationship text, URLs, property sources or arbitrary tokens. It does not
invoke getters or inspect password values. Existing accessible names and page
text can still contain application secrets: the browser tool is not a general
secret scrubber. Existing `ax_tree` identity/name fields are unchanged.

Control states are observations, not actionability checks. The existing native
input helpers still enforce visibility, disabled/inert ancestry, hit testing,
node/document identity and permissions. State can change after collection;
`snapshot` is not an atomic page-wide assertion or a continuous monitor.

## Validation

Eight authored projection regressions cover primitive types, tri-state controls,
unknown/undefined state, duplicate conflicts, limits, disclosure and stable
annotations. An independent Chromium probe confirmed actual accessibility wire
shapes for native and ARIA controls, including numeric busy. That probe is not
Rust-host execution. The implementation environment lacks `dsr`; compilation,
formatting, Clippy and Rust tests require `dsr quality --tool pi_agent_rust` and
remain unexecuted here.

## Snapshot-to-snapshot changes

Every native snapshot also reports `details.snapshot_changes`, schema
`pi.browser.snapshot-changes.v1`, and a textual change summary. The ordinary
full snapshot still follows it; this is not a compressed-output or fewer-CDP-
requests claim. Its baseline is the previous successful native snapshot retained for
that conversation, tab and document/frame. Navigation to a new loader/frame,
first observation, release or reference-cache eviction produces `compared:false`.
A collection/navigation failure does not replace the earlier baseline. An
interruption after native completion but before outer result publication can
already have advanced it; this is not an acknowledged-delivery checkpoint.

A compared result contains ordered `newly_observed`, `changed`, and
`no_longer_observed` reference lists plus `unchanged_count`. A change means a
reported tag, role, bounded accessible name or projected state differs. This
also detects removal of a previously reported property. `order_changed` tracks
inventory ordering; `metadata_changed` tracks page URL/title independently of
control state. Both are null for a fresh baseline.

Only bounded reference IDs and in-memory SHA-256 fingerprints are retained for
comparison, not prior names, state values or URLs. Only IDs and change counts
are emitted: a previous payload is never replayed into the next observation.
This state shares existing conversation/frame ownership and cache lifetime;
no global registry or durable snapshot archive is added.

`coverage_limited` is true if either inventory was truncated or a supported
state projection was incomplete. Even when false, the comparison covers only
the compact, supported observation—not the entire DOM, editable values or all
ARIA attributes. A reference that is no longer observed might be hidden,
ignored, beyond the inventory cap or gone. It is NOT proof of DOM removal.
Changes between samples can be missed, including changes that revert before
the next snapshot. References may be used only while their native document
remains valid, and observed state never overrides the input safety checks.

Five additional unit regressions cover comparison identity, state/name changes,
ordering, baseline resets, bounded retention and incomplete coverage. Two public
HTTP/WebSocket-peer workflows cover the production snapshot path, actual result
shapes, stable references and failure/navigation handling without extra protocol
calls. A separate ignored real-Chromium checkbox lane is also authored. These
Rust tests remain unexecuted; protocol fixtures are not live browser evidence.
