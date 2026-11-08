# Local Browser Companion: `pi web`

`pi web` starts the normal FrankenTUI session with a browser companion on
`127.0.0.1`. The browser shows recent completed conversation text and current
activity. It can propose the next prompt; the terminal operator must review and
accept each proposal with **F8** before Pi starts that work.

The local terminal retains model selection, session management, login, tool
approval, and cancellation. Normal session discovery, trust, extensions, tool
permissions, retry behavior, and spending limits also apply to browser prompts.

## Start and pair

```bash
# Default port 8080, up to four authenticated viewers
pi web

# Show conversation without offering prompt proposals
pi web --view-only --port 9000

# Root options precede the web subcommand
pi --session path/to/session.jsonl --max-cost 2 web --port 0 --max-viewers 2
```

Port `0` asks the operating system for a free port. The private pairing panel
shows the actual address. Open a view or proposal link in a browser on the same
computer, then press Escape to close the panel. Links expire after ten minutes
and can be used once. A full viewer limit does not consume a rejected link.

This mode requires the `ftui` feature and an attached local terminal. It cannot
be combined with `--classic`, print, RPC, ACP, or output/query modes. Only
`--bind loopback` is accepted; LAN, Tailscale binding, headless control, WASM
terminal mirroring, and QR pairing are outside this implementation.

## Review a prompt

The browser sends ordinary text up to 8 KiB. One proposal may wait for local
admission at a time. It expires after two minutes and remains tied to its
original session. Session replacement, browser disconnection, expiration, or
revocation invalidates an unadmitted proposal, including one already queued
after a local key press.

When the editor is empty and available, the terminal shows a waiting proposal.
An existing draft remains untouched; the header advertises the browser inbox.
Press F8 to review it, then F8 in the review panel to accept, or Escape to
reject. Enter, pasted text, and repeated key events cannot approve it.

Accepted text goes directly to the normal native prompt path. Browser text does
not expand `@file` references or templates. Slash and shell-prefixed commands
are rejected. Tool approval dialogs and login prompts remain on the terminal.
Escape cancels an approved prompt even while it is waiting for the driver.

| Local command | Behavior |
|---|---|
| `/web` | Review a waiting proposal, or show new private pairing links |
| `/web pair` | Create new links without changing the conversation |
| `/web inbox` | Review the next waiting proposal |
| `/web revoke` | Revoke clients, unused links, and unadmitted proposals |

A prompt already admitted locally remains an ordinary local turn; use Escape
to cancel it. `/restart` preserves the companion options when resuming the
session. Closing Pi closes the listener and revokes all browser credentials.

## Data and transport boundaries

The browser receives at most 48 recent completed message slots, through a
bounded semantic projection. Complete selected semantic fields undergo the
session's privacy screening before display omission or clipping. Known and
detected credentials are redacted. Tool payloads, reasoning, custom messages,
attachments, terminal drafts, and approval cards are not rendered. A projection
that cannot be screened within its budget is withheld. The server does not
export a raw terminal buffer or live provider deltas.

Pairing uses 256-bit random fragment tokens; only their hashes are retained by
the server. The page clears the fragment and keeps its separate credential in
memory. API requests require authentication and the exact local Origin and
Host. The service has a single owned I/O worker, bounded connection and request
sizes, exchange deadlines, and proposal rate limits. Responses disable caching
and referrers; the page uses a restrictive CSP and text-only DOM insertion.

The optional in-memory audit trail retains at most 128 event records containing
event types and timestamps. It contains no prompts, transcript text, session
IDs, or tokens. There are no persistent `web.*` settings; use the CLI options.

Implementation: `src/web_remote/companion/` and
`src/interactive_ftui/browser.rs`. Regression coverage exercises the actual
loopback listener and the local FTUI/SDK admission path. Runtime validation
belongs to the repository's DSR quality recipe.

## Embedded terminal transport API

The library also exposes `WebRemoteServer` and `WebRemoteBridge` for hosts that
publish terminal frames over WebSockets. That API has its own frame protocol,
pairing credentials, local takeover approval, and sequenced input queue. An
embedding host must publish screened frames, approve control locally, and
revalidate queued input before applying it. Its LAN/Tailscale binding and viewer
limits describe that API; `pi web` currently starts the supervised companion
described above. The two transports do not share credentials or control leases.
