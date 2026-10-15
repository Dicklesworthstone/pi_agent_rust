# Native browser readiness waits

The existing `browser` action `wait_for` supports four states and optional text
and sampled-stability conditions. It reuses the native CDP connection, tool
permissions, conversation ownership and complete-operation deadline.

```json
{"action":"wait_for","tab":"app","selector":"#loading","state":"hidden","timeout_ms":10000}
```

```json
{"action":"wait_for","tab":"app","selector":"#status","text":"Saved successfully","text_match":"exact","stable_ms":300,"timeout_ms":10000}
```

`state` defaults to `visible`. `hidden` succeeds for an absent element or one
that is not visibly rendered. `attached` accepts a connected node even when
hidden; `detached` requires absence/disconnection, not just hidden styling.
Visibility does not mean in-viewport, enabled, unobstructed or clickable. Input
still runs the separate native actionability and hit-test checks.

CSS and known snapshot references support every state. Semantic selectors
(`text/`, `role/`, `label/`, and related forms) support only `visible` waits:
semantic lookup intentionally omits hidden nodes, so its absence cannot prove
DOM detachment. Missing or stale snapshot identities and malformed/ambiguous
selectors are errors, not absence. CSS retains its ordinary first-match behavior.
An unavailable remote node is an error unless the DOM helper actually observes
it detached; protocol errors are never converted to a successful hidden wait.

`text` is a nonempty string of at most 16 KiB and requires `visible` or `attached`.
`text_match` is `contains` (default) or `exact`. Matching is case-sensitive after
Unicode whitespace normalization. Text inspection follows visible DOM text,
open shadow roots and assigned slots, with boundaries between block elements.
It ignores script/style/template content, hidden descendants, and input/textarea
values, including password/autofill values. It does not use HTML, execute caller
expressions, evaluate regular expressions or return the observed page text.
Text nodes must have rendered text boxes; connected but unrendered light-DOM
children beneath a closed shadow root do not match. CSS-generated text and
closed shadow-root contents are outside this text model.
A hidden anchor cannot provide a positive rendered-text observation.

Visibility checks bound composed ancestry to 128 elements. Each text observation
admits at most 20,000 nodes, depth 128 and 1,048,576 UTF-16
text units; traversal errors refuse the observation rather than truncating it
into a potentially false match. These limits do not bound Chromium's layout
work or the browser process's memory.

`stable_ms` is 0–10,000, default 0. Samples must keep matching the same resolved
node (or absence); a mismatch or replacement starts the interval again. The
poll interval is 100 ms plus query time. This is sampled stability, not a
continuous DOM observer: changes entirely between samples can be missed.
No subsequent action is guaranteed to see the same state.

Every wait pins its initial document, including the loader identity. A page or
selected-frame navigation fails the wait rather than observing a new origin
or using the parent frame. Start a new explicit call after navigation. DOM
re-rendering within the same document is supported by re-resolving CSS/semantic
selectors. Known snapshot references continue to identify their original node.

`timeout_ms` remains the whole-operation ceiling, including session admission,
CDP queries, all polling, stability and diagnostics. Waiting never resets or
extends it. Without an explicit value, `wait_for` retains its 5-second default.
Cancellation follows the original owner; no browser input is generated.

Results report `matched`, `found`, `visible`, `textMatched`, `state`, `stable_ms`
and `samples`. `found:false` can be successful for a disappearance condition.
The explicit fixture backend refuses extended conditions instead of simulating
readiness. Malformed conditions and wait-only fields on other actions are
rejected before connection or native input.

## Wait after one input

Attach a `wait_after` object to `click`, `type`, `fill`, or `press`:

```json
{
  "action":"click",
  "tab":"app",
  "selector":"role/button[name=\"Save\" exact]",
  "wait_after":{
    "selector":"#status",
    "text":"Saved successfully",
    "text_match":"exact",
    "stable_ms":300
  },
  "timeout_ms":10000
}
```

The input executes once, then the condition is polled on the **same connection,
original document, selected frame, conversation and original deadline**. The
object accepts only `selector`, `state`, `text`, `text_match` and `stable_ms`;
it cannot override scope, run another action or grant extra time. Invalid field
types, unsupported combinations and size limits are rejected before the input.
Selector syntax or match failures requiring the live document are reported by
the subsequent wait, after the input may have completed. `dialog_response` combinations,
other action kinds and cross-document navigation waits are unsupported.

Success adds `details.wait_after.satisfied:true` and the readiness observation,
while retaining the original action's facts, including pointer acknowledgement
and its `delivery_verified:false`. A matching condition may have existed before
the input; `causality_verified:false` explicitly makes no claim that the input
caused it. The action is never automatically replayed.

An observed postcondition failure returns an unsuccessful tool result retaining
the original action details and a bounded error category. This distinguishes
completed input from an unconfirmed outcome. If the complete-operation watchdog
or owner cancellation interrupts the call, the existing outer error can still
replace the entire result; remote effects may already have occurred. A timeout
does not undo an input and is not permission to repeat it. Inspect the page first.

Native loopback transport tests cover polling, text/stability, malformed replies,
absence, exact pointer counts, navigation before inspection, timeout and owner
cancellation with admission recovery. These fixtures exercise the Rust path when
run, but do not represent a production browser. An additional explicitly selected
Chromium test covers actual click/fill effects and failure without replay.

Both standalone waits and `wait_after` can be ordinary calls inside the existing
`batch` action. A failed postcondition is an unsuccessful step, so the batch
preserves its completed-input details and stops before later steps. Neither
batching nor a postcondition introduces another execution lease or extra time.

## Verification status

Unit/preflight regressions and an explicitly selected real-Chromium public-tool
workflow are included. The implementation host independently exercised the exact
embedded JavaScript helper through real Chromium CDP, not the Rust host. Native
compilation, tests, formatting and Clippy require `dsr quality --tool
pi_agent_rust`; this command was unavailable during implementation. No full OMP
parity, DSR pass or release acceptance is implied.
