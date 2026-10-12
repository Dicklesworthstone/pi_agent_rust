# macOS desktop backend

The opt-in `computer` tool selects this backend on macOS instead of requiring
an X11 display. Linux retains its existing X11/AT-SPI implementation. This is
an implemented source path, not a claim of live macOS verification.

## Requirements and activation

Use macOS 14 or newer and Xcode Command Line Tools with a compatible macOS SDK.
Pi embeds a constant Swift helper and invokes `/usr/bin/swift`; it does not
install a compiler, download an automation daemon, or change macOS permissions.
An SDK host may override `swift` with an absolute trusted helper path through
`ComputerTool::with_helper_path`.

Enable `--tools ...,computer` or `computer.enableComputer: true`.
`computer.requireApproval` remains true by default. Clipboard writes and input
use the same host approval picker as other desktop mutations; a missing or
dismissed picker denies them. Model arguments cannot grant permission.
Observation can expose private desktop data: use a desktop the session is
authorized to inspect.

Screen Recording permission is required for window listing and screenshots.
Grant permission through normal macOS controls to the hosting application or
terminal; no helper requests or bypasses consent automatically. Missing tools,
permissions, unsupported operations and invalid results return errors, never
fixture data. A Swift launcher failure may indicate missing Command Line Tools
or an incompatible SDK; its stderr is deliberately not forwarded.

## Implemented operations

`list_displays` returns active CoreGraphics display IDs, desktop-point bounds,
backing scale and pixel dimensions. IDs are native nonzero 32-bit values, not
one-based array positions. At most 64 displays are admitted. Its
`supported_actions` field advertises the currently available macOS operations.

`list_windows` returns normal on-screen windows in front-to-back order with
native window IDs, owning PIDs, bounded titles/app names and desktop-point
bounds. At most 256 windows are admitted. `frontmost_application` is not a claim
of keyboard focus; minimized and off-screen windows are not in this catalog.

`screenshot` uses ScreenCaptureKit. Choose `display_id` or `window_id`, not both;
with neither, the main display is captured. The PNG is capped at 20 MiB and
128 megapixels. Rust checks its dimensions against helper metadata and publishes
it through the existing confined, create-only artifact writer. `output_path`
must be workspace-relative and end in `.png`; traversal, symlink ancestors and
existing destinations are rejected. The helper never receives this path.
Captures within the inline-image budget also return a native image block.

Screenshot metadata includes desktop-point bounds and independently derived
`pixels_per_point_x` and `pixels_per_point_y`. To map a screenshot pixel back to
the desktop, divide by the corresponding ratio and add the bounds' origin.
Origins can be negative on multi-monitor desktops. The shared tool schema
distinguishes X11 pixels from macOS desktop points.
Retina and mixed-scale mappings still require live verification.

`clipboard_read` returns exact UTF-8 text, bounded to 64 KiB, and rejects a
clipboard change observed during the read. `clipboard_write` accepts at most
4,096 Unicode scalars, requires host approval by default, and confirms the
written text through a read-back. A failed or cancelled write may already have
cleared/replaced the previous clipboard; rollback is not promised.

`ax_tree` inspects the requested window, or the focused window when no ID is
provided. It requires Accessibility permission. Explicit window targeting also
requires Screen Recording permission: the helper correlates the native window
ID with AX using PID, exact unsanitized title and desktop-point geometry. Missing
or ambiguous matches fail rather than selecting a different window. This is
correlation, not an authenticated OS identity boundary.

Trees are bounded to 256 nodes, depth 12, 128 children per node, 64 KiB of labels
and an eight-second traversal budget. A 150 ms accessibility messaging timeout
limits individual API calls. Cycles and exhausted budgets set `truncated`.
Unavailable/stale accessibility data fails explicitly. Value/Text interfaces
are never queried; secure text fields omit titles, geometry and descendants as
well. Other application titles/labels can still contain private data. Rust
checks the tree schema, counts, depth, label budget and redacted-node shape
before returning it to the model.

`focus_window` activates and raises one correlated window, confirms the exact
focused AX object, then re-resolves its native ID before reporting success.
`mouse_move` uses desktop points, admits negative multi-monitor origins, rejects
points outside active displays, and observes the actual pointer position after
posting the event. `scroll` posts a line-based wheel event with the requested
direction and amount; it reports dispatch, not an inferred application effect.
All three retain the existing host approval requirement. When a `window_id` is
supplied for pointer movement or scrolling, that window must already be focused.
Focus can race; coordinates and wheel events are not confined to that window.

## Clicks, dragging, typing and shortcuts

All four input actions require host approval by default and macOS Accessibility
permission. They use an owned, bounded input channel, not an unguarded helper.

```json
{"action":"mouse_click","x":400,"y":240,"button":"left"}
{"action":"mouse_drag","x":600,"y":300,"button":"left"}
{"action":"key_type","text":"Literal Unicode: café 🦀"}
{"action":"key_press","key":"Cmd+Shift+C"}
```

`mouse_click` supports left, right and middle buttons. Coordinates are optional;
with neither it clicks at the current pointer. `mouse_drag` starts at the current
pointer and requires an endpoint. It emits 24 interpolated drag movements before
releasing the button. Every sampled path point must lie on an active display;
points in gaps between monitors are rejected before any press. Results include
the observed final pointer position and the selected button, not a claim that
the application accepted the click or drop.

`key_type` emits literal Unicode through Quartz keyboard events, with one
complete UTF-16 scalar per press/release pair. Combining characters, joiners and
surrogate pairs retain their order. Newline and Tab dispatch Return and Tab;
other control characters and NUL are rejected. Input is limited to 4,096 Unicode
scalars, and empty text emits no events. It never uses the clipboard, shell
interpolation, or an accessibility value setter. Results report `char_count`,
not a copy of the typed text. Application frameworks may interpret keyboard
events themselves instead of honoring their Unicode payloads; successful
dispatch is not verified text insertion.

`key_press` accepts one ANSI-US physical letter/digit, a named navigation key,
or F1 through F20, plus Ctrl, Alt, Shift and Cmd/Super modifiers. Control is an
alias for Ctrl; Command/Meta/Super are aliases for Cmd. Bare `C` adds Shift;
`Ctrl+C` means Ctrl plus the unshifted C key, and `Ctrl+Shift+C` is explicit.
Modifier presses precede the primary key, and releases run in reverse order.
Backspace maps to the backward Delete key, Delete to Forward Delete, and Insert
to the Mac Help key. F21 through F24 and arbitrary key commands are not supported.
Shortcuts use Carbon's physical ANSI-US key positions; use `key_type` rather
than relying on a shortcut key's label for text in another keyboard layout.

Existing held input is not silently cleared: mouse transactions reject a held
target button or active shortcut modifier; keyboard transactions reject held
required keys and active shortcut modifiers, Caps Lock or Fn state. Avoid
concurrent human input. An optional `window_id` is correlated once and must
remain the focused AX window at every event checkpoint. The helper checks its
input lease again after potentially blocking focus queries. This is a focus
precondition, not a security boundary or spatial confinement; a focus change
between the check and event can still affect another application.

## Ownership and verification

Finite inspection/clipboard requests travel as JSON through anonymous-file
stdin. Click/drag/keyboard requests travel as exactly one JSON line (32 KiB
maximum) on a nonblocking pipe; Rust keeps the writer open as the input lease.
The fixed helper source and trusted executable selection are separate from
request data. No text is interpolated into Swift, shell source or argv.

The input helper is the sole event emitter. It checks for lease EOF, a second
command or deadline expiration between events; failures unwind through releases
for only the inputs it holds. Normal and emergency release events are allocated
before each transaction's first press. Typing builds two native events per
scalar transaction rather than retaining thousands of native events. Normal
success is reported only after release dispatch; `released: true` is a dispatch
receipt, not independent observation of the OS key/button state. The application
outcome is always `application_effect: "not_verified"`. Rust checks receipt
fields, requested target and exact text count/chord or button/position before
returning success. Failed input is never automatically repeated.

On cancellation or future drop, Rust closes the input lease first and allows
up to 500 ms of cooperative cleanup before killing the process group and reaping
the helper. It does not spawn a new cleanup process after cancellation and has
no detached stdin/stdout pump threads. The grace is not a guarantee: a crashed
helper, abrupt host termination, stalled OS call or an uncooperative helper can
leave input effects behind. Releases cannot undo already accepted text, clicks
or application actions. Cleanup/reaping is synchronous and not hard real-time.

The helper runs outside the workspace with bounded stdout and discarded
stderr. Capability checks run before spawn and after completion. The existing
tool deadline includes approval, lock waiting, Swift startup and OS work; each
helper has a 30-second bound, and input emission also has a 20-second budget.

Validation in this implementation environment: the combined helper parsed with
Swift's macOS-target frontend. The portable production mouse/keyboard planners
and lifecycle engine passed 1,313 assertions, including every modifier subset,
cancellation at every event boundary, a sink failing after a press, reverse
release order, literal Unicode round-trips and input bounds. These are recording
sink tests, not native desktop tests. The actual combined helper passed eight
Linux framing/protocol checks, including replying while its input lease remains
open, invalid field types, incomplete frames, excess data and private-data-safe
errors. Linux execution checks the explicit unsupported-platform response, not
macOS APIs.

The portable fixture can be run without requesting desktop access:

```sh
probe_dir=$(mktemp -d)
cat src/computer/native/macos_input.swift \
    src/computer/native/macos/input_tests.swift > "$probe_dir/input.swift"
swift -swift-version 5 -D PI_INPUT_TESTS "$probe_dir/input.swift"
```

Rust regression tests cover protocol admission, catalogs, clipboard replies,
PNG identity/dimensions, no-overwrite publication, helper failure, the macOS
dispatch seam, target confirmation, accessibility privacy/budgets, public tool
approval, input pipe ownership and receipt validation. They have not run:
`dsr quality --tool pi_agent_rust` returned `dsr: command not found` (exit 127),
and no Rust toolchain is installed here. Full DSR quality, macOS SDK typechecking
and live permission/input/desktop tests remain required. Neither the portable
Swift fixture nor its Linux protocol probes constitute a DSR quality pass.
