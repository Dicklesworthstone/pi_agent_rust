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
`computer.requireApproval` remains true by default. Clipboard writes use the
same host approval picker as other desktop mutations; a missing/dismissed
picker denies them. Model arguments cannot grant permission. Observation can
expose private desktop data: use a desktop the session is authorized to inspect.

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
Origins can be negative on multi-monitor desktops. The shared tool schema's
legacy pixel wording describes X11; macOS coordinates are desktop points.
Retina and mixed-scale mappings still require live verification.

`clipboard_read` returns exact UTF-8 text, bounded to 64 KiB, and rejects a
clipboard change observed during the read. `clipboard_write` accepts at most
4,096 Unicode scalars, requires host approval by default, and confirms the
written text through a read-back. A failed or cancelled write may already have
cleared/replaced the previous clipboard; rollback is not promised.

Accessibility inspection, focus, pointer and keyboard actions are not yet
implemented in this increment. Unsupported actions fail before helper spawn.

## Ownership and verification

Request data travels as JSON through anonymous-file stdin, never interpolated
Swift or shell source. The helper runs outside the workspace with bounded
stdout and discarded stderr. Capability checks run before spawn and after the
helper finishes. The existing tool deadline includes approval, lock waiting,
Swift startup and OS work. Each finite helper also has the shared 30-second
limit. Cancellation drops the owned process; already accepted OS effects are
not transactional.

Source validation in this implementation environment: the helper parsed with
Swift's macOS-target frontend and executed its explicit non-macOS rejection
path on Linux. These are not a macOS SDK typecheck or native desktop tests.
Rust regression tests cover protocol admission, catalogs, clipboard replies,
PNG identity/dimensions, no-overwrite publication, helper failure and the macOS
dispatch seam. They have not run: `dsr quality --tool pi_agent_rust` returned
`dsr: command not found` (exit 127), and no Rust toolchain is installed here.
Full DSR quality and actual macOS permission/desktop tests remain required.
