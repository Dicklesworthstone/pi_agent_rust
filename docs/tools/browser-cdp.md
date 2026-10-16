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
session; selecting a tab in an admitted CDP-created context also applies denial
to that context before attachment/input. Use the explicit `download` action below
to capture one transfer. These policies are context-wide, not network isolation.

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
not CSS selectors, element references or frame names. Use the returned ID rather
than trying to select a browser-wide iframe target directly.

```json
{"action":"list_frames","tab":"work"}
```

The result contains `frames`, with each entry's `frame_id`, `parent_id`,
`is_main`, `out_of_process`, `allowed`, `url` and `name`. Names and URLs are omitted
for frames blocked by the URL policy. Discovery recursively auto-attaches only
iframe targets related to the selected page, without pausing them for a debugger.
It merges the local frame trees of those renderers through validated parent-frame
and parent-session links; another tab or a worker cannot join the inventory.
Discovery does not itself enter an isolated DOM world or change future calls'
document scope. The combined tree is limited to 256 frames and 32 nesting levels,
with a separate bound on attachment work; malformed or changing trees fail
explicitly rather than returning a silently incomplete inventory.

Use an actual returned frame ID in place of `FRAME_ID` below:

```json
{"action":"snapshot","tab":"work","frame":"FRAME_ID"}
{"action":"fill","tab":"work","frame":"FRAME_ID","selector":"#email","text":"person@example.com"}
{"action":"press","tab":"work","frame":"FRAME_ID","selector":"#email","key":"End"}
{"action":"click","tab":"work","frame":"FRAME_ID","selector":"@e12"}
{"action":"scroll","tab":"work","frame":"FRAME_ID","selector":".results","delta_y":400}
{"action":"wait_for","tab":"work","frame":"FRAME_ID","selector":".result","timeout_ms":10000}
{"action":"evaluate","tab":"work","frame":"FRAME_ID","script":"({title: document.title, count: document.forms.length})"}
{"action":"upload","tab":"work","frame":"FRAME_ID","selector":"input[type=file]","files":["reports/result.csv"]}
{"action":"download","tab":"work","frame":"FRAME_ID","selector":"a.export","output_path":"downloads/report.csv"}
```

An explicit `tab` is required whenever `frame` is supplied. The frame option is
per-call: it is never a persistent switch that could silently redirect a later
operation. Both same-process and nested out-of-process frames support `snapshot`,
`ax_tree`, `evaluate`, `click`, `type`, `fill`, `press`, `scroll`, `wait_for`,
`upload` and `download`. Scoped keypress, wheel and file-transfer actions require
an explicit selector instead of using ambient focus, the main viewport or an
implicitly selected control. Native input actions bring the selected page
to the front. Keyboard/text actions focus the requested control in its renderer
and check focus before native input dispatch to the page.

CSS lookup runs in the selected frame's isolated world. Runtime, DOM and
accessibility commands use that frame's owning renderer session; native input
uses the page session. Responses must match both the request ID and renderer
session. Snapshot references must be used with the same tab and frame; inspecting
the parent or another frame does not overwrite them. The registry retains at most
256 document buckets. Evicted, detached and navigated references fail rather than
rebinding to replacement nodes. Snapshot details include `frame_id` and
`reference_scope`.

For same-process clicks and wheel input, Pi obtains Chromium's content quads in
page-viewport coordinates, clips them to the viewport, and verifies the actual
hit-test node belongs to the selected frame and target element or its composed
descendants. The DOM hit-test lookup includes the renderer's document scroll
offset; the native mouse/wheel coordinates remain in the viewport.

For an out-of-process frame, the same leaf check produces a renderer-local
candidate. Pi projects that candidate through each embedding iframe's **content
quad**, using the child's full viewport dimensions, including scrollbar space.
Border boxes, padding offsets, and client-width-only scaling are not substituted
for this mapping. A projective transform handles nondegenerate convex quads,
including CSS scaling, rotation, reflection and perspective. Each parent renderer
must then hit the exact embedding iframe in its expected parent document at the
projected point. Fractional projections check the enclosing integer hit-test grid
and retain fractional coordinates for actual native input.

An overlay in an ancestor document therefore blocks the operation even when the
frame's own local hit test sees an unobstructed element. Frame-local DOM rectangle
coordinates are never blindly reused as page coordinates. Geometry, frame and
candidate counts are bounded; malformed, degenerate, folded, occluded or
out-of-viewport projections fail without pointer dispatch. A pinch-zoomed or
visually offset viewport is explicitly unsupported for cross-process pointer
input, rather than approximated. Failure never falls back to JavaScript clicks.

Each scoped operation pins the selected frame and its ancestors' IDs, loader IDs,
URLs and renderer sessions before entering an isolated world. Related renderer
attachments carry incarnation identities, so a detach/re-attach cannot reuse an
old selection merely by repeating an opaque session ID. Every ancestor must pass
the URL policy; nested `about:blank` and `about:srcdoc` cannot bypass a blocked
ancestor. Navigation, reparenting or detach detected during the operation is an
error, not permission to enter the new document. Unrelated sibling reloads do not
invalidate an otherwise unchanged selected ancestry. These checks are not an
atomic browser transaction: page scripts can run between protocol commands, and
cancellation or failure cannot roll back effects already dispatched. The
operation's socket owns all its renderer attachments and drops them on exit.

Frame-scoped evaluation has DOM access in an isolated world, not access to the
page's JavaScript global variables. Frame options on exports, navigation,
tab/lifecycle actions or dialog responses are rejected before connection or
launch. Those existing operations remain main-document or tab-level operations.
Chromium errors never authorize a fallback to the main frame.

## Select real files for upload

```json
{"action":"upload","selector":"input[type=file]","files":["reports/result.csv"]}
{"action":"upload","selector":"@e12","files":["images/first.png","images/second.png"]}
{"action":"upload","selector":"input[type=file]","files":[]}
{"action":"upload","tab":"work","frame":"FRAME_ID","selector":"input[type=file]","files":["reports/result.csv"]}
```

The selector must identify an enabled ordinary file input in the main document
or the explicitly selected frame, including a local child inside a remote renderer.
An explicit frame always requires `tab` and `selector`; it never redirects a later call.
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
The exact selected document and its policy are rechecked after lookup, before
local reads, after staging and before selection. The original backend node's type,
availability and file-count constraints are inspected again after staging; the
selector is not resolved a second time. Ancestor navigation or renderer changes
fail rather than authorizing the renderer's root or a replacement control.
Scoped `about:srcdoc` inherits the already-validated ancestor policy. Selection
is verified on the same document and node, and results include `frame_id`.
These checks are not atomic with page scripts and do not roll back exposure.

Copies are retained before sending the CDP selection command, including when
cancellation races its acknowledgement. Browser File objects can read them in
a later call or after navigation, so copies are not prematurely deleted when a
tab closes or another selection replaces them. Retention is bounded to 64 MiB
and 32 batches per session; exceeding the limit fails instead of evicting files
that may still be in use. Finish pending transfers before stopping the managed
browser or dropping the tool to release copies. Attached-browser users must
finish such transfers before ending the Pi tool session as well.

### Frame-upload validation status

Nine Rust test functions were added across document/preflight checks, public
HTTP/WebSocket upload scenarios and an explicitly ignored live Chromium scenario.
They were authored but not executed: `dsr quality --tool pi_agent_rust` was
attempted and returned command not found (exit 127). Compilation, formatting,
Clippy, Rust tests and the public live-browser scenario remain unverified.
A separate Chromium 144.0.7559.96 protocol/helper probe passed 22 checks for local,
remote and remote-local file inputs, literal bytes, equal filenames, clearing,
disabled controls and parent preservation. It observed an actual remote renderer,
but did not execute the Rust adapter. Its root-container browser used
`--no-sandbox` and `--site-per-process`; production launch flags are unchanged.

## Capture a browser download

```json
{"action":"download","selector":"a.export","output_path":"downloads/report.csv"}
{"action":"download","selector":"@e8"}
{"action":"download","tab":"work","frame":"FRAME_ID","selector":"a.export","output_path":"downloads/frame-report.csv"}
```

`download` temporarily enables Chromium's `allowAndName` behavior in a private
directory, performs one native left click through the existing pointer path,
and waits for one completed transfer. CSS selectors and snapshot references are
supported; semantic selectors and click modifiers are not accepted by this action.
The optional `frame` requires an explicit `tab` and `selector`, including for a
local child inside a remote renderer or nested remote renderer chains. Omission
selects the main document. Frame download triggers use the same native coordinate
projection and ancestor hit tests as other scoped clicks, never JavaScript clicks.

Pi pins the initiating frame ID, loader ID and URL before enabling capture.
`Browser.downloadWillBegin.frameId` must match that exact frame; another tab,
parent, child or sibling does not qualify merely because it downloaded the same
URL. The selected document and, for scoped calls, its complete ancestry and
renderer incarnations must remain unchanged through completion and publication.
Navigation is an error, not an instruction to accept a replacement document.
HTTP(S) download hosts must pass `domainAllowlist` independently of the source
page. Client-generated `blob:` and `data:` exports use the resource-specific
admission described below; navigation and CDP endpoint permissions are unchanged.

The complete set of newly observed transfers is checked during progress and
again after restoring `deny`. Multiple transfers, including a second transfer
observed during cleanup, cause refusal rather than choosing one silently.
Already-observed transfers before the capture window are neither candidates nor
cancellation targets. A new transfer observed before starting the click path
prevents that click. Source-frame matching and timing are correlation, not proof
that a particular page handler caused a download: page scripts can run between
protocol commands. No input is automatically retried.

A transfer's source frame, URL and suggested filename cannot change after its
first event. Exact duplicate start notifications do not reset progress. Progress
is validated before changing retained state, received-byte counts cannot retreat,
and terminal states cannot reopen or change byte counts. Invalid events latch a
failure across best-effort helper cleanup, so a discarded cleanup error cannot
become permission to publish later. Total-byte estimates may change while the
transfer is in progress; exact duplicate terminal notifications are accepted.

Captured transfers are limited to 100 MiB using both Chromium progress and the
actual completed file size. A canceled, missing, symbolic, oversized, or
byte-count-mismatched file is rejected. Zero reported bytes must match an actually
empty file. With no `output_path`, the suggested filename is sanitized and
published under `downloads/`; supplied paths must be workspace-relative and use
the same descriptor-pinned create-only publisher as screenshots/media artifacts.
Existing files and symlinked ancestors are never overwritten or followed.
Results include `saved_path`, `size_bytes`, `received_bytes`, a safe display `url`,
`url_kind`, `url_redacted`, `suggested_filename`, the verified source `frame_id`
and the derived `browser_context` described below.

After a normal attempt Pi restores download behavior to `deny` before publishing.
An error during capture enablement also enters this cleanup path: a lost or failed
acknowledgement does not prove the policy was unchanged. On ordinary failure it
attempts cancellation of transfers first observed in this capture window without
replaying the click. Cleanup shares the original deadline and cancellation. If
outer cancellation interrupts cleanup, the session retains the original context
for recovery on the next connected operation. This is not immediate rollback of
network, page or browser effects. No failed capture publishes a file.
Download capture does not inspect file contents or assert they are safe to open.

Download policy is browser-context-wide, not per frame or per tab. Existing tabs
in an admitted CDP-created context are supported as described below. Use a
dedicated browser without unrelated simultaneous downloads in the same context;
`domainAllowlist` remains an access guard, not a subresource/network sandbox.

### Existing browser contexts and interrupted-capture recovery

The ordinary `tab` selection also supports a page in an existing context created
through `Target.createBrowserContext`. Use its page target ID from `list_tabs`:

```json
{"action":"list_tabs"}
{"action":"download","tab":"PAGE_TARGET_ID","selector":"#export","output_path":"downloads/context-report.csv"}
```

Replace `PAGE_TARGET_ID` with an actual returned page ID. No context ID is supplied
by the model. The selected target's `browserContextId` is checked against the
bounded `Target.getBrowserContexts` inventory, and target/context identity is
rechecked alongside the source-document checks through publication. Malformed,
unlisted or changed context identities fail rather than redirecting the call.
The existing absent-context-field protocol form retains default semantics.

An admitted named context receives `deny` before attachment and input, in addition
to the existing default-context initialization. Capture enablement, reset and
cancellation then carry the same named context ID. A named policy error is never
retried against the default profile or another named context. Frame-scoped
transfers still use the page's context even when DOM operations use a different
renderer session. The 100 MiB limit, file-integrity checks, URL policy, native
input projection and create-only publisher are unchanged.

Some Chromium versions report an ordinary default-profile ID in target metadata
but neither list it as a CDP-created context nor accept it in browser download
commands. Pi uses `defaultBrowserContextId` when the inventory supplies it.
Otherwise, for an unclassified target, it creates one hidden, background
`about:blank` target with no context override, reads its default identity and
closes that probe target. The probe belongs to the operation's CDP connection;
disconnection also ends its lifetime. It does not navigate or close a user tab.
Failure to establish or close the probe is an error, not permission to assume
that an arbitrary unlisted profile is the default. The probe can be observed by
other debugging clients even though it has no visible tab.

Results contain `browser_context: {"kind":"default"}` or
`browser_context: {"kind":"named","id":"..."}`. These are browser context
identifiers, distinct from numerical JavaScript execution-context IDs.
This increment does not create or dispose user browser contexts, create new tabs
inside a selected context, or change storage-state access. A new `open` tab name
still creates a default-context tab. Manually created incognito/profile contexts
not exposed as CDP-created contexts remain unsupported; no failed selection
falls back to a personal or unrelated browser profile.

Before enabling a capture, the owning session retains its original context as a
pending recovery obligation. Following an error or interrupted cleanup, the next
connected call restores denial there before listing/selecting a different tab or
performing other browser work. A failed inventory or reset retains the obligation
and blocks the later action. An original CDP-created context that is no longer
in the inventory is treated as disposed; recovery is not redirected elsewhere.
Changing the endpoint while recovery is pending is refused before a new connection.
Stopping a Pi-owned browser destroys its contexts and clears their retained state.

Recovery is best effort under each call's existing deadline, not a background
worker or an immediate rollback. Releasing a namespace or dropping an attached
tool cannot asynchronously restore a remote policy after its connection is gone;
the externally attached browser remains the operator's responsibility. Pages can
continue running while no tool call is active. Different clients sharing the
same context can still change its policy. No file contents or application effects
are made transactional by the context checks.

Five unit tests and ten public HTTP/WebSocket test functions were authored for
this increment. The public cases reuse the existing download peer and include
nested frame captures, reported/probed default identity, unknown/malformed
contexts, changed ownership, named-policy errors, a withheld reset acknowledgement,
recovery across repeated connections, failed recovery retention, disposed contexts
and endpoint-switch refusal. They were not compiled or executed: the required
`dsr quality --tool pi_agent_rust` attempt returned command not found (exit 127).
Rust compilation, rustfmt, Clippy, native adapter and cross-platform acceptance
remain unverified.

A separate offline Chromium 144.0.7559.96 protocol probe passed 23 cases: eighteen
literal binary/Unicode/empty downloads across the default context and two named
contexts, in main documents and local children; a negative control proving that
default-only enablement does not capture a denied named-context download;
independent named-context policy, reconnect denial, hidden-target disconnect
cleanup and disposed-context inventory. Each successful transfer checked trusted
native input, exact source frame, unchanged document, byte count and saved bytes.
This did not execute the Rust adapter or validate remote-renderer context captures.
The root-container probe used `--no-sandbox`; production flags and managed browser
policies were unchanged. No DSR pass or Bead closure is claimed.

### Client-generated exports and URL redaction

The same `download` call works when an export link or its click handler creates
a Blob or data resource. The model selects the original control, not a replacement
resource URL. Chromium produces the bytes; Pi does not fetch the URL itself,
decode a second payload copy, synthesize a download event or relax source checks.

For `blob:`, the embedded creator URL must use HTTP(S), have a nonempty handle,
and pass the existing exact/wildcard/IDNA host policy without URL credentials.
The creator need not be the initiating document, but both must independently be
admitted. Opaque `blob:null/...`, nested/non-HTTP creator schemes and Blob URLs
with query strings remain unsupported. A revoked or otherwise failed Blob
transfer is an error, not permission to substitute another URL.

For `data:`, the URL must have its metadata/payload comma separator. There is no
remote resource host to authorize; the pinned initiating document and all scoped
ancestors still pass their ordinary policy. Thus an allowed `about:blank` page
can export literal data even with an empty network-host allowlist. This does not
permit navigation to data URLs or downloading resources from a blocked source.

All browser-reported resource URLs remain bounded to 64 KiB of UTF-8, with no
control characters or surrounding whitespace. The existing 100 MiB completed-byte
limit is independent; large client-side exports should use Blob resources rather
than oversized inline data URLs. Unsupported resources fail with a redacted
`BROWSER_DOWNLOAD_URL` diagnostic after the browser has reported the transfer.

The original URL remains private to immutable transfer correlation. A changed
raw URL is rejected even when both versions would display the same redacted text.
Success text and `details.url` instead contain:

- HTTP(S): the parsed URL without query or fragment.
- Blob: `blob:<creator-origin>/[redacted]`, without the object handle.
- Data: `data:[redacted]`, without MIME parameters or payload.

`url_kind` is `http`, `https`, `blob` or `data`; `url_redacted` records whether the
display differs from the original. Normalization can also set that flag. This is
URL projection, not a general secret sanitizer: HTTP paths, suggested filenames,
page content and the saved file may still contain application-sensitive data.

This increment adds ten authored Rust tests: five policy tests and five public
transport test functions. The transport cases retain real loopback I/O, binary
file publication, native-input counts and restoration checks while covering
client-generated resources, offline blank sources, creator refusal, payload
redaction, late/rebound resources, foreign frames, byte mismatches and URL limits.
They were not compiled or executed: the required DSR command was attempted and
returned command not found (exit 127).

A separate offline Chromium 144.0.7559.96 probe passed twelve data-download cases:
base64, percent-encoded binary/Unicode and empty payloads in the main document,
a local child, an observed out-of-process srcdoc renderer and its local child.
Each case checked a trusted native click, exact source-frame event, unchanged
document, completed byte count and literal saved bytes. It did not execute the
Rust adapter or validate HTTP-origin Blob transfers. An initial HTTP fixture
failed because this environment's managed browser policy blocks HTTP navigation;
the policy was left unchanged. Probe-only `--no-sandbox` and `--site-per-process`
did not change production launch flags. No DSR or Rust test pass is claimed.

### Source-bound download validation status

The source/transfer guard and frame-download increments add 15 Rust test functions:
seven state/document regressions, one frame-admission regression and seven public
preflight/transport test functions in `tests/browser_downloads.rs`. The transport
cases cover main documents, same-process children, remote roots, local children
inside remote renderers and two remote boundaries. They check literal binary
bytes, single-node lookup, one native click pair, policy restoration before
publication, duplicate notifications, zero-byte files, unrelated or late transfers,
changed identities, poisoned cleanup, navigation, failed enablement/press/reset,
file-integrity failures and existing destinations. They are fixtures for the Rust
adapter, not a simulated browser substituted into production.

These tests were authored but not executed. The required
`dsr quality --tool pi_agent_rust` command was attempted and returned command not
found (exit 127). Compilation, rustfmt, Clippy, Rust tests and the public browser
workflow remain unverified. An independent Chromium 144.0.7559.96 experiment did
not establish the intended remote iframe and therefore supplied no live download
validation. That standalone root-container experiment used `--no-sandbox` and
`--site-per-process`; production launch flags were unchanged. No Beads were closed.

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

### Cross-process pointer increment validation status

This increment adds twelve Rust test functions: three scrolled hit-test
regressions, six geometry/viewport unit tests, two additional public-tool
HTTP/WebSocket protocol tests, and the explicitly ignored real-Chromium scenario
in `tests/browser_remote_pointer.rs`. The earlier pointer-refusal fixture is now
a positive projection test: it requires two renderer boundaries, distinct border
and content quads, scrolled leaf/parent documents and the actual page-input route.
Negative controls cover ancestor occlusion, malformed geometry, navigation during
projection, zoomed viewports and a partial-pixel overlay. The live Rust scenario
requires a genuinely out-of-process sandboxed srcdoc frame, stable element
references, trusted click/wheel events, CSS transforms, scroll offsets and
ancestor-overlay refusal; it never treats missing Chromium as a passing test.

The Rust tests were authored but **not executed** here. The required
`dsr quality --tool pi_agent_rust` command returned `dsr: command not found`
(exit 127); Rust compilation, rustfmt, Clippy and DSR remain unverified.

A separate CPython CDP probe against Chromium 144.0.7559.96 exercised an offline
sandboxed srcdoc frame in a separate renderer. It observed six trusted native
clicks and five trusted wheel events across untransformed, scaled, rotated,
perspective-transformed and independently scrolled parent/child cases. A parent
overlay was detected even though the child's own hit test still found the button.
It also reproduced the scrolled-hit-test defect: with child scrollY=800, the
viewport point (105,120) found no DOM node, while document point (105,920) found
the intended button. This validates the protocol/geometry assumptions, **not**
the compiled Rust implementation or multi-remote-renderer execution. The standalone
root-container probe used `--no-sandbox` and `--site-per-process`; production
launch flags were not changed. No release, quality-gate or Bead-closure claim is
made from that probe.

### Earlier cross-process frame increment validation status

The cross-process increment adds 18 regression test functions across frame
discovery, session/ancestry admission, response correlation and a real loopback
HTTP/WebSocket protocol peer. The peer drives the public browser tool for nested
frame listing, evaluation, snapshots, form filling, focused keypresses and waits;
it also injects navigation, detach, foreign-target, blocked-ancestor and
wrong-session conditions. It validates the Rust adapter's intended transport
behavior, not Chromium's renderer, hit testing or actual native input.

These tests were authored but **not executed** in the implementation environment.
`dsr quality --tool pi_agent_rust` was unavailable (exit 127). No Rust compile,
formatting, Clippy, test pass, live cross-process Chromium run or DSR gate result
was claimed for that earlier increment. Pointer projection was implemented in
the subsequent increment above.

### Earlier same-process frame increment validation status

The earlier frame increment adds 16 Rust test functions: frame-tree/policy/document
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
