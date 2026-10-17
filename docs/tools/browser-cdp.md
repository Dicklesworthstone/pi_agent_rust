# Native browser automation: managed Chromium and CDP attachment

Feature tracking: `bd-cv653.2.4`.
Implementation: `src/browser.rs` and `src/browser/`.

The browser tool sends real Chrome DevTools Protocol commands over the existing
asupersync HTTP/WebSocket transports. Production never substitutes canned
results, fabricated navigation statuses or a one-pixel screenshot.

## Start an isolated browser

Enable the browser tool through the existing setting or selected tool list:

```json
{"browser":{"enableBrowser":true}}
```

```sh
pi --tools read,write,edit,bash,browser
```

The default native connection now lazily launches an installed Chromium/Chrome
executable. No separate browser-start command is necessary. Pi creates a private
temporary profile, asks the OS for an ephemeral loopback debugging port, reads
`DevToolsActivePort`, and checks discovery against that profile's debugger path.
It does not use the operator's ordinary Chrome profile, install a browser,
automatically disable the sandbox, or accept arbitrary browser flags from the
model. The launcher searches trusted absolute PATH entries and standard macOS
and Windows installation locations.

Host environment controls:

```sh
PI_BROWSER_EXECUTABLE=/usr/bin/chromium \
PI_BROWSER_HEADLESS=false \
PI_BROWSER_USER_AGENT='Pi browser automation' \
  pi --tools read,write,edit,bash,browser
```

`PI_BROWSER_HEADLESS` defaults to true and accepts `true`, `false`, `1` or `0`.
A headed browser can be used for an interactive login in the temporary profile.
Cookies and login state survive between calls in that same tool session, not
across stopping or dropping the owned browser. Running Chromium as root without
its required OS sandbox support may fail; Pi reports the startup error instead
of silently adding `--no-sandbox`.

SDK hosts can set `BrowserLaunchOptions` through `with_launch_options`. Explicit
SDK connection choices take precedence over the environment. Stop an existing
managed browser before changing its connection configuration.

**Settings forwarding boundary:** the existing registry still forwards browser
activation and `domainAllowlist`, but not the legacy `executablePath`, `headless`,
`userAgent` or `remoteDebuggingPort` fields. Use the environment variables above
or SDK launch options. For a fixed, externally managed port, use explicit CDP
attachment rather than asking the managed launcher to share that port.

## Lifecycle and explicit attachment

```json
{"action":"status"}
{"action":"start"}
{"action":"open","tab":"work","url":"https://example.com"}
{"action":"stop"}
```

`start` launches or attaches and verifies the CDP connection. Ordinary actions
also launch lazily. `status` never starts a process: it reports an idle managed
browser as stopped, or probes the existing managed/attached connection.
`stop` terminates only a Pi-owned browser and releases its temporary profile and
upload staging. It is idempotent for an idle managed session. Dropping the tool
also attempts process-tree cleanup before deleting its temporary files.

A failed or cancelled startup owns its process locally until discovery and the
WebSocket handshake succeed, so it is cleaned up rather than published as a
usable session. An established browser belongs to the session, not to the
completed first call's cancellation context. Cancelling a later operation drops
that operation's socket; it does not roll back browser effects or automatically
kill the whole established browser. If the process exits, the next operation
reports lost tabs before a retry can start a fresh process. Element reference
IDs are not reused across restarts.

To attach to an already-running dedicated browser:

```sh
PI_BROWSER_CDP_URL=http://127.0.0.1:9222 \
  pi --tools read,write,edit,bash,browser
```

SDK callers use `with_cdp_endpoint`. An explicitly configured endpoint disables
automatic launch; a connection failure does not fall back to another browser.
Only IPv4 loopback and `localhost` are accepted. Remote hosts, URL credentials,
endpoint query strings, and discovery responses changing the port are rejected.
`stop` refuses to terminate an attached browser because Pi does not own it.
The external browser remains the operator's responsibility after Pi exits.

The CDP endpoint grants browser control. Never expose it to a network or point
it at a personal browsing profile merely for convenience.

## Navigation, inspection and input

```json
{"action":"open","tab":"research","url":"https://example.com"}
{"action":"snapshot","tab":"research"}
{"action":"fill","selector":"#search","text":"structured concurrency"}
{"action":"click","selector":"@e3"}
{"action":"press","key":"Ctrl+a"}
{"action":"wait_for","selector":".results","timeout_ms":10000}
{"action":"evaluate","script":"({title: document.title, count: document.links.length})"}
```

`open` creates a named blank target when needed and navigates it. Reusing a name
navigates that target. `goto` requires an existing tab. `list_tabs` returns actual
page targets; an unclaimed target can be addressed by its target ID. `close`
closes the real target. Active means the tool's selected tab, not desktop focus.

Navigation waits for the matching document's DOMContentLoaded/load lifecycle
event. Downloads and navigation errors are not reported as successfully loaded
pages. Browser downloads are denied by default for each established tool
session so an ordinary click/evaluate/keypress cannot silently write an unmanaged
file. Use the explicit `download` action below to capture one transfer.

`snapshot` produces up to 200 accessibility-backed element references.
`ax_tree` also returns up to 1,000 actual accessibility nodes with truncation
metadata. References identify backend DOM nodes in one document; replacing or
detaching a node, or navigating to a new document, makes old references unusable.
Names are captured rather than editable field values. Explicit frame selection
is described below; omitting `frame` preserves main-document behavior.

Clicks use native mouse events after visibility and obstruction checks. `type`
inserts native input; `fill` replaces and verifies the retained text. Read-only,
disabled, detached and unsupported text controls fail explicitly. Keyboard
chords support Ctrl/Control, Alt, Shift and Meta/Cmd/Command. `scroll` accepts
`delta_x`/`delta_y` CSS-pixel deltas, defaulting to 0/600. `wait_for` polls for
visibility and actually times out. Input helpers use an isolated world and
structured CDP arguments rather than interpolated executable source.

## Inspect and operate an embedded frame

First obtain the frame ID from the selected tab. Frame IDs are browser-generated,
not CSS selectors, element references, frame names or target IDs.

```json
{"action":"list_frames","tab":"work"}
```

The result contains `frames`, with each entry's `frame_id`, `parent_id`,
`is_main`, `allowed`, `url` and `name`. Names and URLs are omitted for frames
blocked by the URL policy. Discovery does not itself enter a frame or change
future calls' document scope. The returned tree is limited to 256 frames and
32 nesting levels; an oversized or malformed tree fails explicitly.

Use an actual returned frame ID in place of `FRAME_ID` below:

```json
{"action":"snapshot","tab":"work","frame":"FRAME_ID"}
{"action":"fill","tab":"work","frame":"FRAME_ID","selector":"#email","text":"person@example.com"}
{"action":"press","tab":"work","frame":"FRAME_ID","selector":"#email","key":"End"}
{"action":"click","tab":"work","frame":"FRAME_ID","selector":"@e12"}
{"action":"wait_for","tab":"work","frame":"FRAME_ID","selector":".result","timeout_ms":10000}
{"action":"scroll","tab":"work","frame":"FRAME_ID","selector":".results","delta_y":400}
{"action":"evaluate","tab":"work","frame":"FRAME_ID","script":"({title: document.title, count: document.forms.length})"}
```

An explicit `tab` is required whenever `frame` is supplied. The frame option is
per-call: it is never a persistent switch that could silently redirect a later
operation. Supported scoped actions are `snapshot`, `ax_tree`, `evaluate`,
`click`, `type`, `fill`, `press`, `scroll` and `wait_for`. Scoped keypress and
wheel actions also require an explicit selector, instead of using ambient focus
or the main viewport. Keyboard/text actions bring the selected page to the front,
focus the requested control and check focus before native input dispatch.

CSS lookup runs in the selected frame's isolated world. Snapshot references must
be used with the same tab and frame; inspecting the parent or another frame does
not overwrite them. The registry retains at most 256 document buckets. Evicted,
detached and navigated references fail rather than rebinding to replacement
nodes. Snapshot details include `frame_id` and `reference_scope`.

For clicks and wheel input, Pi obtains Chromium's content quads in page-viewport
coordinates, clips them to the viewport, and verifies the actual hit-test node
belongs to the selected frame and target element or its composed descendants.
An overlay in an ancestor document therefore blocks the operation even when the
frame's own local hit test sees an unobstructed element. Frame-local DOM rectangle
coordinates are never blindly reused as page coordinates. Geometry and candidate
counts are bounded, and no mouse event is dispatched when checks fail.

Each scoped operation pins the selected frame and its ancestors' IDs, loader IDs
and URLs before entering an isolated world. Every ancestor must pass the URL
policy; nested `about:blank` and `about:srcdoc` cannot bypass a blocked ancestor.
Navigation, reparenting or detach detected during the operation is an error,
not permission to enter the new document. These checks are not an atomic browser
transaction: page scripts can run between protocol commands, and cancellation
or failure cannot roll back effects already dispatched.

**Current scope:** same-process frames reachable through the attached page's CDP
session. Out-of-process iframe attachment/routing is not implemented. Chromium
errors when a frame cannot be entered are propagated without a main-frame
fallback. Frame-scoped evaluation has DOM access in an isolated world, not access
to the page's JavaScript global variables. Frame options on uploads, downloads,
exports, navigation, tab/lifecycle actions or dialog responses are rejected
before connection or launch. Those existing operations remain main-document or
tab-level operations.

## Select real files for upload

```json
{"action":"upload","selector":"input[type=file]","files":["reports/result.csv"]}
{"action":"upload","selector":"@e12","files":["images/first.png","images/second.png"]}
{"action":"upload","selector":"input[type=file]","files":[]}
```

The selector must identify an enabled ordinary file input in the main frame.
Hidden file inputs are supported because many sites hide them behind styled
buttons. Multiple inputs require the element's `multiple` attribute. Directory
upload controls are rejected. An empty list clears the selection through the
native input setter and input/change events; Chromium 144 treated an empty
`DOM.setFileInputFiles` list as a no-op in the implementation probe.

Paths are relative to the tool's working directory. Absolute paths, parent
traversal, symbolic-link components and non-regular files are rejected. On
supported Unix targets, files are opened through pinned parent descriptors
with no-follow semantics and copied under a private staging directory. This
confined source-opening path is not implemented for Windows or the exceptional
Unix targets without the required descriptor APIs; those calls fail explicitly.

A call accepts at most ten files and 20 MiB of actual source bytes. Private copies
preserve order and filenames, including equal filenames in different source
directories. Source files are not modified. The browser receives the staged
paths, and Pi verifies the resulting FileList's names and byte sizes.

**Selecting files exposes them to page scripts immediately.** It is not a
preview or a promise to wait for a separate submit button. The tool itself does
not click submit, but the page may automatically read or transmit the files.
The current document URL is rechecked against the allowlist before local reads
and before selection; document changes and unexpected selections fail.

Copies are retained before sending the CDP selection command, including when
cancellation races its acknowledgement. Browser File objects can read them in
a later call or after navigation, so copies are not prematurely deleted when a
tab closes or another selection replaces them. Retention is bounded to 64 MiB
and 32 batches per session; exceeding the limit fails instead of evicting files
that may still be in use. Finish pending transfers before stopping the managed
browser or dropping the tool to release copies. Attached-browser users must
finish such transfers before ending the Pi tool session as well.


## Capture a browser download

```json
{"action":"download","selector":"a.export","output_path":"downloads/report.csv"}
{"action":"download","selector":"@e8"}
```

`download` temporarily enables Chromium's `allowAndName` behavior in a private
directory, performs one native click through the same selector/reference path as
`click`, correlates the resulting `Browser.downloadWillBegin` and
`Browser.downloadProgress` events, and waits for completion. Multiple transfers
from one click fail rather than choosing one silently. The final download URL is
checked against `domainAllowlist` before publication.

Captured transfers are limited to 100 MiB using both Chromium progress and the
actual completed file size. A canceled, missing, symbolic, oversized, or
byte-count-mismatched file is rejected. With no `output_path`, the suggested
filename is sanitized and published under `downloads/`; supplied paths must be
workspace-relative and use the same descriptor-pinned create-only publisher as
screenshots/media artifacts. Existing files and symlinked ancestors are never
overwritten or followed.

After a normal download attempt Pi restores browser download behavior to
`deny` before publishing the artifact. If cancellation interrupts that cleanup,
the session records the policy as uninitialized and the next operation reapplies
`deny` before proceeding. This prevents cancellation from leaving a persistent
unmanaged download directory. Download capture does not inspect file contents or
assert they are safe to execute/open.

## Screenshots and PDF exports

```json
{"action":"screenshot","output_path":"screenshots/viewport.png"}
{"action":"screenshot","full_page":true,"output_path":"screenshots/full-page.png"}
{"action":"print_pdf","landscape":true,"print_background":true,"page_ranges":"1-3,5","output_path":"exports/report.pdf"}
```

Screenshots capture actual PNG pixels. `full_page` uses CSS content metrics and
captures beyond the viewport, bounded to 32,768 pixels per side and 128
megapixels. `print_pdf` uses Chromium's native Page.printToPDF command, honors
CSS page size, and supports orientation, background graphics and page ranges.
An unsupported browser or invalid page range returns the actual command error.

Both formats are capped at 20 MiB decoded bytes. Basic container/completion
checks reject missing or malformed output; these are not full PDF/PNG decoders
or a general artifact sanitizer. Files are staged inside a descriptor-pinned workspace directory, synced and
published create-only without following ancestor symlinks or clobbering an
existing destination, including concurrent creations. Absolute paths, parent
traversal and platform separator escapes are rejected. A failed request does not
publish a partial artifact.

`output_path` must end in `.png` or `.pdf` as appropriate. Omitting it creates a
unique file under `screenshots/` or `exports/`. Small PNGs also return an image
content block; captures exceeding the 4.5 MiB inline-image budget remain on disk
with an explicit note. PDF results contain the path and metadata, not another
base64 copy in the conversation. Export paths are not a filesystem sandbox.

## Boundaries and validation

`timeout_ms` covers the asynchronous operation, including launch and lock wait
(default 30 seconds, `wait_for` 5 seconds, maximum 120 seconds). Browser startup
also has its own 30-second readiness bound. Blocking local filesystem calls and
process reaping are not preemptible hard real-time operations. Cancellation
cannot undo bytes already exposed to a page, network requests, or browser input.

`domainAllowlist` matches parsed hosts, not URL substrings: `example.com` is
exact, `*.example.com` permits proper subdomains, and `*` permits HTTP(S) hosts.
An empty list denies network navigation; `about:blank` is allowed. This is a
navigation/access guard, not network isolation. Page scripts, subresources and
`evaluate` can generate traffic; use an OS/container network policy for strict
isolation. The tool declares read, write, network and process effects.

Mock mode requires `with_mock(true)` or `PI_BROWSER_MOCK=1`;
`with_mock(false)` overrides the environment. New lifecycle, frame, upload,
full-page capture and PDF operations deliberately reject mock mode rather than
inventing successful process, input or transfer results.

### Frame increment validation status

The frame increment adds 16 Rust test functions: frame-tree/policy/document
identity tests, geometry and handle-admission tests, public preflight checks,
and one ignored real-Chromium integration scenario in `tests/browser_frames.rs`.
That live scenario exercises iframe fill/type/keypress, actual native clicks,
ancestor overlay rejection, parent-value preservation, reference scoping and
stale/detached frames. It requires an installed sandbox-capable Chromium and
explicit selection by the DSR browser lane; missing Chromium is not a fake pass.

These tests were authored but **not executed** in the implementation environment.
`dsr quality --tool pi_agent_rust` exited 127 because DSR was not installed;
Rust tooling was absent as well. No build, test pass or browser execution result
is claimed for the frame increment.

### Earlier lifecycle/file-workflow evidence

The lifecycle/file-workflow increment adds 18 Rust regression test functions
across the browser modules and `tests/browser_cdp.rs`, including actual loopback
HTTP/WebSocket fixtures. They were authored but **not executed** in the
implementation environment: neither Rust nor DSR was installed. The required
command failed with `dsr: command not found`, exit 127:

```sh
dsr quality --tool pi_agent_rust
```

An independent Chromium 144 probe passed 13 protocol/helper checks: file-input
metadata, literal staged bytes, retained selections across reconnects, clearing,
disabled/non-file rejection, isolated-world behavior, full-page pixels and native
PDF bytes. The tested helper blob is `dc124b2d96239ef35ea1e95a01289d5faaf0f614`.
It used about:blank fixture content, not a live website. Because this container
runs as root, that separate test browser explicitly used `--no-sandbox`; the
production launcher never adds that flag. A probe of the unchanged production
launch flags failed with Chromium's root/sandbox error as expected. None of
these probes establishes a passing Rust adapter build, managed-launch happy path,
DSR gate, cross-platform support result, release or Bead closure.

Protocol references: [Chrome remote debugging](https://developer.chrome.com/blog/remote-debugging-port),
[CDP Page definitions](https://github.com/ChromeDevTools/devtools-protocol/blob/master/pdl/domains/Page.pdl),
and [CDP DOM definitions](https://github.com/ChromeDevTools/devtools-protocol/blob/master/pdl/domains/DOM.pdl).
