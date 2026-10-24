# Browser Automation Tool: `browser`

> **Tool family:** Headless Chromium Automation (Opt-in)  
> **Bead ID:** `bd-cv653.2.4`  
> **Module:** `src/browser.rs`

## Overview

The native browser tool launches an isolated Chromium browser or attaches to an
explicit loopback CDP endpoint. It supports tabs, navigation, DOM/accessibility
snapshots, native input, structured forms, file transfers and page exports.
See [the native browser guide](browser-cdp.md) for lifecycle, frame selection,
file limits and attachment details.

Enable the tool using JSON settings or the explicit tool list:

```json
{"browser":{"enableBrowser":true,"domainAllowlist":["example.com"]}}
```

```sh
pi --tools read,write,edit,bash,browser
```

The domain allowlist is a navigation/access guard, not a network sandbox. Page
scripts and subresources can generate traffic. Use an operating-system or
container network policy for network isolation. Browser startup options belong
to the host, not model-supplied action arguments.

## Text, focus and native keys

`type`, `fill` and targeted `press` resolve a selector once and retain its original
document. They bring the selected tab forward and verify that the original
control owns focus before native insertion. A focus handler redirecting to
another control, or navigation during preparation, causes refusal rather than
re-resolving a selector in a replacement document. These checks also apply to
the main page, not just iframe inputs.

```json
{"action":"fill","tab":"app","selector":"#email","text":"person@example.com"}
{"action":"press","tab":"app","selector":"#email","key":"Enter"}
```

Native key pairs do not resend keydown after failure. An uncertain keydown
acknowledgement gets one best-effort keyup attempt on the original page; a child
frame's navigation cannot redirect the release. Cleanup shares the original
cancellation/deadline. Main-page Enter/Tab can intentionally navigate or move
focus; explicit `wait_after` controls outcome observation.

## Date/time, color and range filling

`fill` also accepts ordinary HTML `date`, `time`, `month`, `week`,
`datetime-local`, `color` and `range` inputs. `type` does not append to these
specialized controls. Use HTML value strings, not locale-specific picker labels:

```json
{"action":"fill","tab":"app","selector":"#date","text":"2026-10-16"}
{"action":"fill","tab":"app","selector":"#time","text":"12:34"}
{"action":"fill","tab":"app","selector":"#month","text":"2026-10"}
{"action":"fill","tab":"app","selector":"#week","text":"2026-W42"}
{"action":"fill","tab":"app","selector":"#appointment","text":"2026-10-16T12:34"}
{"action":"fill","tab":"app","selector":"#color","text":"#ABCDEF"}
{"action":"fill","tab":"app","selector":"#level","text":"25"}
```

An empty string clears date/time/month/week/datetime-local, even if the field is
required. Color and range cannot be cleared. Color is limited to opaque
`#RRGGBB`; controls with `alpha` or `colorspace` attributes are refused. Range
values must be finite decimal numbers. Clamping or rounding a requested range
value to another number is refused rather than silently accepted.

Before touching the original field, Pi validates on a detached ordinary input
using Chromium's value normalization and the original `min`, `max`, `step` and
value-attribute step base. Invalid dates and range/step violations are rejected.
Specialized input text is bounded to 4,096 UTF-16 code units and each inspected
constraint attribute to 256. Read-only, disabled, inert and hidden controls remain
unsupported. After focus handlers run, the same node/type/constraints are checked
again before mutation.

These fills use the isolated world's DOM value setter and untrusted
`input`/`change` events, **not trusted keyboard input or physical picker clicks**.
Already-matching values are no-ops without repeating those events. A subsequent
protocol command verifies the same original node, type and canonical value after
handler microtasks. Rejected values, changed controls and navigation never cause
native-text fallback or automatic replay.

Results include `input_method: "dom-value"`, `input_type`, `changed` and
`value_normalized`; requested and retained field values are not copied into the
result. Ordinary text inputs continue using native insertion. The same selected
frame routing applies with `frame` plus an explicit `tab`; the operation never
falls back to the main document. The shared `wait_after` path can observe an
additional same-document postcondition.

Input is not an atomic browser transaction. Page handlers can run between
commands, may transmit values immediately, and cannot be rolled back. Preparing
ordinary number fills can clear the old value before a later focus check fails.
A reported failure does not mean the page was untouched.

## Validation status for this increment

Fifteen Rust regression functions were added across the shared input module and
`tests/browser_text_input.rs`, including an explicitly ignored live Chromium
scenario. They were authored, not compiled or executed: the required
`dsr quality --tool pi_agent_rust` command was unavailable (exit 127).

A separate 52-case Chromium protocol/helper probe passed, covering the original
focus-diversion reproduction, guarded refusal, ordinary fields, specialized
normalization/idempotency/clearing, invalid constraints, focus changes, detached
nodes and synchronous/microtask reversion. It used the exact DOM helper but did
not execute the Rust adapter. Its root-container browser required `--no-sandbox`;
production launch flags were unchanged. No DSR or cross-platform pass is claimed.
