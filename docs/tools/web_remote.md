# Browser access: `pi web`

Both browser modes run the normal local FrankenTUI agent, preserving session
discovery, workspace trust, credentials, extensions, tool policy, retry behavior,
and spending limits. The selected mode owns one listener and one set of browser
credentials; control or credentials from one mode cannot authorize the other.

| Mode | Invocation | Browser input | Network |
| --- | --- | --- | --- |
| Supervised companion | `pi web` | Each proposed prompt needs local approval. | Loopback only. |
| Terminal mirror | `pi web --terminal` | A local operator grants a revocable control lease for a separate prompt composer. | Loopback, explicit Tailscale, or explicit LAN. |

Both require a build with the `ftui` feature and an attached local terminal.
Headless operation, WASM rendering, and scannable QR pairing remain unimplemented.
Root options such as `--session`, `--model`, and `--max-cost` precede `web`.

## Supervised companion

The default companion shows recent completed conversation text and current
activity. The terminal operator reviews each proposed prompt before Pi starts
that work. Model selection, session management, login, tool approval, and
cancellation remain local.

```bash
# Default port 8080, up to four authenticated viewers
pi web

# Show conversation without offering prompt proposals
pi web --view-only --port 9000

# Resume a session with a budget and an automatically selected port
pi --session path/to/session.jsonl --max-cost 2 web --port 0 --max-viewers 2
```

Open a view or proposal link from the private local pairing panel in a browser
on the same computer, then press Escape to close the panel. Links expire after
ten minutes and can be used once. A full viewer limit does not consume a
rejected link. Companion mode accepts only `--bind loopback` and viewer limits
from 1 through 16, with a default of 4.

### Review a prompt

The browser sends ordinary text up to 8 KiB. One proposal may wait for local
admission at a time. It expires after two minutes and remains tied to its
original session. Session replacement, browser disconnection, expiration, or
revocation invalidates an unadmitted proposal, including one already queued
after a local key press.

When the editor is empty and available, the terminal shows a waiting proposal.
An existing draft remains untouched; the header advertises the browser inbox.
Press F8 to review it, then F8 in the review panel to accept, or Escape to
reject. Enter, pasted text, and repeated key events cannot approve it.

Accepted text goes directly to the normal native prompt path without expanding
`@file` references or templates. Slash and shell-prefixed commands are rejected.
Tool approval and login prompts remain local. Escape cancels an approved prompt
even while it is waiting for the driver.

| Local command | Companion behavior |
| --- | --- |
| `/web` | Review a waiting proposal, or show new private pairing links. |
| `/web pair` | Create new links without changing the conversation. |
| `/web inbox` | Review the next waiting proposal. |
| `/web revoke` | Revoke clients, unused links, and unadmitted proposals. |

A prompt already admitted locally remains an ordinary local turn; use Escape
to cancel it. `/restart` preserves the companion options when resuming the
session. Closing Pi closes the listener and revokes browser credentials.

### Companion data and transport

The browser receives at most 48 recent completed message slots through a
bounded semantic projection. Complete semantic fields undergo privacy discovery
before selecting that display tail, omitting fields, or clipping text. This
keeps a bare credential echo protected even when its original assignment is
outside the displayed history. Screening is bounded to 256 KiB and 4,096 source
items; a projection that exceeds its protection budget is withheld.

Known and detected credentials are redacted. Tool payloads, reasoning, custom
messages, attachments, terminal drafts, and approval cards are not rendered.
The companion does not export terminal cells or live provider deltas.

Pairing uses 256-bit random fragment tokens; only their hashes are retained by
the companion server. The page clears the fragment and keeps its separate
credential in memory. API requests require authentication and the exact local
Origin and Host. A single owned I/O worker enforces bounded connections,
requests, exchange deadlines, and proposal rates. Responses disable caching and
referrers and use a restrictive CSP and text-only DOM insertion.

The optional in-memory companion audit trail retains at most 128 event records
containing event types and timestamps. It excludes prompts, transcript text,
session IDs, and tokens. There are no persistent `web.*` settings; select browser
behavior with CLI options.

Implementation: `src/web_remote/companion/` and
`src/interactive_ftui/browser.rs`. Its regression tests exercise the actual
loopback listener and local FTUI/SDK admission. DSR owns runtime validation.

## Terminal mirror

`pi web --terminal` starts the native agent and an authenticated HTTP/WebSocket
server. A paired browser sees a text projection of the same terminal frame and
can submit prompts after the local operator approves control. The ordinary
workspace trust, model, session, extension, and tool-policy startup paths still
apply.

This mode requires a build with the `ftui` feature and a local terminal on both
stdin and stdout, using the fullscreen interface. Headless and inline operation
are not supported. The current browser
client is JavaScript with a monospace terminal projection; WASM rendering,
terminal colors/images in the browser, and scannable QR pairing are not
implemented. Pairing uses the copyable URL in the host's F8 panel.

### Start and pair

```bash
# Local browser on the same machine; default port 8080
pi web --terminal

# Pair over the host's local Tailscale IPv4 address
pi web --terminal --bind tailscale --port 9000

# Permit viewing only, regardless of a client's requested control
pi web --terminal --bind tailscale --view-only

# Ask the operating system to choose an available port
pi web --terminal --port 0
```

1. Start the command in the host terminal and complete normal workspace/model
   startup.
2. Open the pairing link shown in the initial local browser-access panel.
   **F8** reopens or closes this panel later. The link
   contains a random one-use credential and expires ten minutes after creation.
3. To type remotely, choose **Request control** in the browser. Review the
   pending request in the host's F8 panel and press **Y** to approve or **N** to
   deny it. Connecting with a steering link does not grant control by itself.
4. Create another link with **P** for another browser or after a disconnect.
   Use **V** for a link that only permits viewing.

The server binds before the agent driver starts. An occupied port reports an
error instead of displaying a pairing link for a nonexistent listener. Leaving
the native session shuts down its listener and connections.

### Terminal options

| Option | Behavior |
| --- | --- |
| `--port <PORT>` | Defaults to `8080`; `0` selects an available port. The F8 link contains the actual port. |
| `--bind loopback` | Default. Binds `127.0.0.1`; admits that address and `localhost` for the bound port. |
| `--bind tailscale` | Uses a locally bindable Tailscale IPv4 address. If Tailscale is missing or has no usable address, reports the fallback and binds loopback. An occupied Tailscale port remains an error. |
| `--bind lan` | Explicitly binds all IPv4 interfaces. The pairing link uses a detected local IPv4 address when available. |
| `--view-only` | Disables all remote steering, including with a steering token. |
| `--max-viewers <N>` | Defaults to `4`; accepts `1` through `64` simultaneous authenticated browsers. |

The server serves HTTP/WebSocket directly. Tailscale supplies network encryption
when that bind mode is used; LAN mode uses the local network's transport. Exact
Host and Origin checks intentionally reject arbitrary hostnames, proxy origins,
and URLs carrying query credentials. Use the URL shown by the host.

`--print`, explicit `--mode`, ACP, `--classic`, `--inline`, export, and model-list/fetch modes
cannot be combined with this interactive mode. Pipe-based launches fail with a
local-terminal requirement before starting a server.

### Local and remote control

The host's **F8** panel is local administration: its pairing URL is painted after
the browser snapshot is captured. **P** and **V** create fresh links, **Y/N**
resolve the displayed request, **R** revokes control, and **F8/Esc** closes the
panel. Host keyboard, paste, or mouse activity reclaims the session. Opening
the administration panel also revokes an active browser controller. A control
request can only be approved when its complete identity and approval hint fit
the local panel; resize a small terminal before approving.

Remote typing uses a separate native text editor. An unsent local draft and its
undo history remain intact, and the local draft stays hidden from browsers.
The approved remote draft is visible in the shared remote interface. The browser
supports ordinary text, cursor/editing
keys, plaintext paste, Enter to submit, Shift/Alt+Enter for a newline, and Escape
to clear the remote draft. Drafts and individual pastes are limited to 16 KiB.
The **Stop turn** button aborts the current turn without quitting the session.
Browser font-size controls affect only that browser's display; terminal
dimensions follow the host.

The browser's status and completed transcript update as the session runs.
In-flight assistant/thinking text stays local until it becomes a completed
transcript entry, allowing credential checks to see the full text before any
partial token is displayed remotely.

Remote input accepts ordinary prompts. Slash commands, shell shortcuts, local
keybindings, and extension shortcuts are not available through this input path.
Prompts remain literal text; `@file` references and templates are not expanded.
Editing pauses while the agent is working or the host has a private dialog.
The configured tool policy still applies to a remote prompt: tool approvals
required by that policy must be answered locally. Browser input cannot answer
tool/extension questions, workspace trust, login, or credential dialogs, or
change the approval policy.

Control is checked when an input is admitted, when the native interface consumes
it, and again before a queued remote prompt enters the agent driver. A local
reclaim, controller release/disconnect, or revocation of its token invalidates
the previous control generation, including its queued input. An unrelated
viewer's departure leaves the active controller's draft intact.

The local `/web` and `/web inbox` commands open terminal administration too.
`/web pair` creates a fresh link. `/web revoke` disconnects viewers and revokes
unused links as well as control; **R** in the panel only reclaims control.

### Terminal privacy and connection lifecycle

Pairing secrets are 256 random bits. The URL fragment is removed from browser
history immediately and sent in the first WebSocket application message, never
in the HTTP request URL. The server sends no terminal frames before successful
authentication. A consumed link cannot reconnect; create a new link locally.
Capacity and duplicate-client rejection do not consume an otherwise valid
credential.

The browser has no application storage, service worker, analytics, external
scripts, or remote assets. Responses use `no-store`, a strict Content Security
Policy, and a no-referrer policy. Authentication instructions and local question
history are hidden from the browser projection. Other frames are screened for
known session credentials, pairing secrets, configured secret patterns, and the
shared built-in detectors before diff generation. Protection includes wrapped
and overlapping known values and retains observed credentials across session
changes. Source blocks are also checked before wrapping and viewport clipping;
sensitive blocks are withheld from the browser even
when only a fragment would have been visible. Cached decisions are tied to both
source revisions and privacy revisions. Truncated live tool cards are
conservatively kept local because discarded source text cannot be screened
afterward. If protection state is unavailable or exceeds its bounded budget, the
browser receives a withholding notice. Normal conversation content remains
visible to paired viewers.

The in-memory `pi.web.audit.v1` ledger stores bounded connection, token,
control, and input-sequence metadata. It excludes raw credentials and keystroke
contents, retains at most 1,024 events, and counts dropped older events. This
implementation does not persist or expose the ledger through a browser API.

### Terminal transport and implementation

The native UI publishes without waiting on a browser. The bridge retains one
current full frame and one immediate-generation line patch. An up-to-date
viewer receives a patch when it is smaller; a lagging or resynchronizing viewer
receives the latest keyframe. There is no growing per-viewer frame backlog.

Snapshots are bounded to 1 MiB and 1,000 × 500 terminal cells. Inbound WebSocket
messages are bounded to 32 KiB, native input queues to 64 events, and command
admission to 120 messages per second per connection. Unauthenticated sockets
have a five-second hello deadline and writes have a two-second deadline. The
owned server joins incomplete HTTP requests, unauthenticated upgrades, and live
viewers on shutdown.

| Component | Responsibility |
| --- | --- |
| `src/web_remote.rs` | Token/client/control state, bounded audit, and embedded browser assets. |
| `src/web_remote/bridge.rs` | Provenance-bearing input, credential protection, and bounded frame publication. |
| `src/web_remote/server.rs` | HTTP/WebSocket admission, network limits, delivery, and listener lifetime. |
| `src/interactive_ftui.rs` | Native frame capture, separate remote editor, local approvals, and agent-driver admission. |

The F10 planning beads (`bd-cv653.10.*`) include broader WASM, headless, and QR
work. Their historical completion status should not be read as evidence that
those remaining capabilities are shipped.
