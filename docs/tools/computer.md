# Native desktop automation: `computer`

Feature tracking: `bd-cv653.2.5`.
Implementation: `src/computer.rs` and `src/computer/`.

The production path calls desktop helpers and returns their actual results.
Connection failures, missing helpers, invalid targets and denied input are
errors, never invitations to substitute fixture windows, canned accessibility
nodes or a one-pixel screenshot.

## Platform and activation

Native operations are available through **Linux X11** and the
[macOS backend](computer-macos.md). macOS requires macOS 14 or newer and Xcode
Command Line Tools; its source path implements display/window discovery,
screenshots, clipboard access, accessibility trees, focus, mouse movement,
clicks, dragging, scrolling, Unicode typing and key chords. See that backend's
guide for permissions, desktop-point coordinates, input-release limitations and
validation status. Live macOS behavior remains unverified.
Windows and native Wayland adapters are not implemented.

The Linux-specific details below describe **X11**. It requires the
session's authorized `DISPLAY` and, where applicable, `XAUTHORITY`. Window
operations require an EWMH-capable window manager. A detected Wayland session
is rejected rather than silently inspecting only its XWayland subset.

Register the tool with `--tools ...,computer` or the existing opt-in setting:

```json
{
  "computer": {
    "enableComputer": true,
    "requireApproval": true
  }
}
```

The Rust binary does not install its desktop dependencies. Linux uses:

| Operations | Helpers |
|---|---|
| Monitor enumeration / monitor screenshot selection | `xrandr --listactivemonitors` |
| Window enumeration and focus metadata | `wmctrl`, `xprop` |
| Screenshots | `scrot` with PNG stdout, window and rectangle support |
| Mouse, keyboard, scrolling and window activation | `xdotool` with XTEST and `type --file -` support |
| System clipboard | `xclip` |
| Accessibility inspection | `python3`, PyGObject and the Atspi 2.0 introspection package, plus a functioning session accessibility bus |

Helpers are resolved through the trusted host PATH. SDK callers may override
one known helper with an absolute path using `with_helper_path`; model-facing
arguments cannot select executables or set a PATH. In particular, the selected
Python interpreter must have the GI bindings installed; an unrelated virtual
environment may not have them.

## Approval and privacy

Input, clipboard writes, scrolling and window activation require a trusted
host approval handler by default. The SDK hook is
`ComputerTool::with_approval_handler(AskHandler)`. It presents the exact action
and accepts only the matching question's explicit **Allow once** choice.
Dismissals, absent handlers, malformed replies and errors deny the action.
There is no recommended-answer or model-supplied `confirmed` fallback.

The default registry routes this callback through the session's host picker.
Interactive CLI/RPC/FTUI hosts install that picker even when the model-facing
`ask` tool is not enabled, so narrowing the model tool list does not silently
disable desktop consent. Headless/print hosts install no interactive picker and
therefore continue to fail closed. SDK sessions created through
`create_agent_session` expose the same picker from `AgentSessionHandle::ask_tool`;
a custom prebuilt `ComputerTool` can still install its own trusted callback.
A host/operator may explicitly grant session-wide input with
`computer.requireApproval: false`. This is a permission grant, not a setting
that tool arguments can change. Other host approval layers still apply
independently.

Enabling this tool grants desktop observation: screenshots, window labels,
accessibility names and clipboard reads can expose private data to the model.
Use an isolated automation desktop and avoid concurrent human input. A
`window_id` check is a focus precondition, not confinement: focus can race and
pointer coordinates can point outside that window. This is not an OS or
filesystem sandbox.

## Inspection and screenshots

```json
{"action":"list_displays"}
{"action":"list_windows"}
{"action":"screenshot","output_path":"screenshots/current.png"}
{"action":"screenshot","window_id":2097186}
{"action":"screenshot","display_id":1}
```

Monitor IDs are one-based XRandR monitor indices from the current listing.
Monitor metadata includes desktop offsets, dimensions and the primary flag.
Coordinates use the X11 desktop pixel space; `scale_factor: 1` denotes that
identity coordinate space, not a measured panel DPI or toolkit scaling factor.
Window IDs are the actual X11 IDs, returned as integers. Titles and class
labels are sanitized and bounded for display.

Choose a window or a display for a screenshot, never both. With neither, the
whole X11 screen is captured. Actual PNG bytes are staged, synced and published
without overwriting an existing destination, including a concurrent creation
or symlink at that destination. Captures are capped at 20 MiB and 128 megapixels.
Container checks are not a full PNG decoder or a general image sanitizer.

Screenshots within the 4.5 MiB inline-image budget also return an image content
block for the model. Larger captures are saved with a note to inspect the file.
The default destination is `screenshots/desktop_<id>.png`; `output_path` must be
workspace-relative and use `.png`. The legacy `screenshotDir`
field is not forwarded by the current registry; use `output_path` explicitly.

## Input and clipboard

```json
{"action":"focus_window","window_id":2097186}
{"action":"mouse_move","x":400,"y":240}
{"action":"mouse_click","x":400,"y":240,"button":"left"}
{"action":"mouse_drag","x":600,"y":300,"button":"left"}
{"action":"key_type","text":"Literal text, including $(shell-looking syntax)"}
{"action":"key_press","key":"Ctrl+Shift+C"}
{"action":"scroll","direction":"down","amount":3}
{"action":"clipboard_write","text":"Text to copy"}
{"action":"clipboard_read"}
```

X11 mouse coordinates must be nonnegative. Movement and dragging require both
coordinates; dragging starts at the current pointer position. Clicks may omit
both coordinates to use that position. Pointer movement and window activation
are checked after dispatch. Input results report dispatched events, not an
inferred application-level outcome.

Text is passed literally on stdin, not interpolated into shell source or helper
arguments. It is limited to 4,096 Unicode characters. Typing permits newline
and tab but rejects other control characters. Clipboard text may contain other
controls except NUL. Key presses accept one validated alphanumeric/function/
navigation key and optional Ctrl, Alt, Shift or Super modifiers. Conventional
`Ctrl+C` means `ctrl+c`; Shift must be explicit. Arbitrary xdotool commands and
command chains cannot be supplied through the key field.

Before key/button presses, a separate authorized release helper is armed. On
failure or cancellation the input process is stopped before release is
attempted. Completion disarms the helper. Cleanup is best effort and cannot
undo input already accepted by the desktop; modifier restoration and other
applications' reactions are not transactional.

Clipboard writes use foreground `xclip`, verify the exact bytes through a
read-back, and retain the selection owner under tool state. It lasts until
another owner replaces it or the tool is dropped; it is not a detached daemon.
Reads are UTF-8 and bounded to 64 KiB. A failed write does not promise restoration
of the previous clipboard contents.

## Accessibility

```json
{"action":"ax_tree","window_id":2097186}
```

Omitting `window_id` selects the currently focused window. The adapter resolves
its reported PID and exact unsanitized title, then requires one matching
AT-SPI top-level window. Missing PID/title, absent application exposure,
ambiguous matches and inaccessible candidates fail rather than guessing a
target. PID/title correlation is not an authenticated OS identity boundary.

The returned tree contains actual roles, names, enabled/focused state and
children. The helper does not query editable Text/Value interfaces; password
names and descendants are omitted. Accessible names themselves can still
contain private application text.

Inspection is bounded to 512 nodes, depth eight and 64 children per node, with
an additional escaped-JSON byte budget. Truncation and omitted-child counts
are explicit. Stale children are omitted with a partial marker; an unavailable
root is an error. Rust independently validates the returned schema, counts,
label bounds and absence of editable values.

## Lifetimes and validation status

`timeout_ms` bounds the asynchronous operation, including approval and lock
waiting (default 30 seconds; maximum 120 seconds). Each finite helper also has
a 30-second bound. Cancellation drops owned subprocesses and attempts cleanup.
Blocking OS operations/reaping are not hard real-time; no deadline can roll
back desktop effects already accepted.

The tool declares read, write and process effects. Its audit ring retains the
latest 256 attempts, outcome and bounded metadata, not typed text, clipboard
contents, raw titles or image bytes. Helper stderr is discarded at spawn to
avoid private-text diagnostics and persistent-service pipe backpressure.

`with_mock(true)` or `PI_COMPUTER_MOCK=1` explicitly selects deterministic test
fixtures, marked `mock: true`. `with_mock(false)` overrides the environment.
Fixture data is never used as a live failure fallback.

Twenty-two Rust test functions were authored for the initial X11 modules, including
approval boundaries, no-fallback behavior, subprocess limits, release guards,
parsers, no-clobber screenshots and accessibility validation. They were **not
executed** in the implementation environment: neither a Rust compiler nor DSR
was installed. The required entry point remains:

```sh
dsr quality --tool pi_agent_rust
```

The attempted invocation failed with `dsr: command not found` (exit 127).
Independent probes exercised real Xvfb/scrot/wmctrl operations and the constant
release script (five checks). The actual embedded Python helper also passed
ten checks with synthetic Accessible objects, including missing-GI failure;
its tested Git blob is `dbcc8f8371f26376fe4e958e4d7b69e1fbd59fb7`.
Those probes are not compiled Rust adapter tests. Live xdotool input, xclip
selection ownership, live AT-SPI, full DSR quality and cross-platform behavior
remain unverified. No release or Bead-completion claim follows from this work.
