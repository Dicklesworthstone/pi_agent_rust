# Private browser authentication state

The native `browser` tool provides `save_state` and `restore_state` for reusing
an authenticated HTTP(S) origin after an owned browser has been stopped or a
conversation's browser namespace released. Both actions use the existing native
CDP transport, conversation ownership lease, configured domain policy and total
operation deadline. There is no second browser runner or guest-code bridge.

## Save and resume

Open the application in Pi's managed browser and authenticate through the normal
application workflow. Then save the explicit tab:

```json
{"action":"save_state","tab":"app","state_path":".pi/private-auth/app.json"}
```

The destination must be a **new workspace-relative JSON file**. Existing files
are not replaced. On supported Unix hosts, publication uses the shared pinned,
no-follow artifact publisher with private permissions and atomic no-clobber
publication. The file contains plaintext credentials. Keep it out of source
control, logs, shared workspaces and uploaded artifacts. Pi does not silently
edit ignore rules, encrypt the file, or manage credential rotation for you.

After stopping the browser, open the intended origin in a fresh managed browser
using the ordinary `open` action. Restore to that explicit tab:

```json
{"action":"restore_state","tab":"app","state_path":".pi/private-auth/app.json"}
```

The document's canonical origin must match the saved origin exactly, including
scheme and effective port. Restore does not open a page, follow a URL from the
file, launch a browser or navigate automatically. A login redirect to another
origin is not accepted as the intended destination. Navigate to a same-origin
page that can remain loaded while state is installed, then deliberately reload
or navigate after restore to let the application consume the state.

`storage_verified: true` means read-back confirmed the requested storage values
and cookie attributes. `authentication_verified` remains `false`: credentials
may have been revoked server-side, require another factor, or be insufficient
for an application. Check the application's authenticated state separately.

## What is persisted

The native format is `pi.browser.storage-state.v1`, containing one canonical
`origin`, `cookies`, and `localStorage`. It is not an import format for arbitrary
Playwright or OMP profile dumps and does not claim full OMP browser parity.

Capture includes matching unpartitioned cookies, including HttpOnly cookies,
with their domain/path, secure flag, SameSite policy, session/persistent expiry
and supported priority/source metadata. Host-only cookies are not widened to
subdomains. Secure cookies are not captured for an HTTP origin. The cookie's
own domain must also pass `domainAllowlist`; an exact subdomain allowance does
not implicitly authorize a broader parent-domain cookie.

Cookie commands run on the selected page's CDP session so its storage partition,
not an unrelated default browser context, is used. localStorage operations use
an explicit origin-bound storage ID, not JavaScript copied from the state file.

Cookie values and localStorage keys/values are never included in save/restore
responses or their error diagnostics. Save returns the destination and counts;
restore returns counts and verification flags. The ordinary browser `evaluate`
and other tools have their own disclosure rules; these actions are not a general
secret scrubber or protection against another process running as the same user.

## Restore semantics and failures

Restore reads the complete bounded file and validates its schema, origin,
cookie scopes, expiries and entry limits before mutation. Duplicate JSON
contract fields, duplicate cookie identities and duplicate storage keys are
errors, not last-writer-wins overrides. Expired persistent cookies fail preflight
rather than being silently discarded.

Restore **merges** the saved cookies and keys. It updates matching identities,
leaves unrelated cookies and localStorage keys alone, and never issues a blanket
clear. After writes it reads both stores back. A missing value, changed cookie
scope/attributes, browser rejection or concurrent application mutation fails
verification rather than producing an unqualified success.

There is no transaction spanning browser cookies and localStorage. Save is also
not an atomic snapshot of a live application's changing state. Frame, loader and
security-origin checks detect observed document changes between operations; they
do not freeze scripts or prevent every navigation race. The existing owner
lease and cancellation watchdog remain in force. Partial effects can remain on
failure, cancellation, timeout or future drop. Nothing is automatically retried,
rolled back or applied to a replacement document.

An operation that fails after restore admission reports `BROWSER_STATE_PARTIAL`;
outer cancellation/deadline errors retain their existing remote-side-effects
warning. Inspect the selected origin before continuing instead of blindly
retrying a failed sequence.

## Deliberate boundaries

Both actions require an **already-running Pi-owned browser** and an explicit
control-free tab name/target ID. Attached external CDP profiles are refused before
connection. Mock mode never simulates a successful save or restore. Actions are
standalone; they are not admitted as native batch steps in this implementation.

Frames, diagnostics, JavaScript dialog responses, model-selected launch settings,
partitioned/same-party cookies, sessionStorage, IndexedDB, service-worker state,
cache storage and browser-profile copying are outside this format. Matching
unsupported cookie scopes cause refusal rather than being silently downgraded.

Persistence currently requires the native Unix file backend (excluding ESP-IDF
and Redox). Other platforms receive an explicit unsupported error rather than
an unproven private-file fallback. Restore requires a singly linked regular file
with no group/world permissions. Absolute paths, parent traversal, backslashes,
symlink ancestors, symlink leaves, FIFOs and other nonregular sources are refused.
Reads use the opened descriptor and are checked for observed concurrent changes.

Limits are 2 MiB of encoded state, 256 selected cookies, 1,024 localStorage entries,
4 KiB names/keys and 128 KiB values, within the browser transport's existing
response bounds. Serialization stops at the file budget rather than allocating
an arbitrarily large final JSON buffer. Whole-operation `timeout_ms` retains the
normal 1–120,000 ms range, including connection and queue time.

## Validation

Unit/regression cases are in `src/browser/storage.rs` and
`src/browser/storage/restore.rs`. They cover schema routing, cookie scope and
metadata, malformed/expired inputs, privacy and file bounds, no-clobber
publication, read-back mismatches, and preservation of unrelated state.
The opt-in live lane uses an installed managed Chromium and a local HTTP fixture
for a stop/start/restore round trip with an HttpOnly cookie and localStorage.

These tests were authored, **not executed** in the implementation environment.
`dsr quality --tool pi_agent_rust` could not run because `dsr` was not installed
(exit 127). Rust compilation, formatting, Clippy, runtime tests, cross-platform
acceptance and real Chromium behavior remain unverified. DSR remains the required
quality entry point; there is no Cargo or GitHub Actions substitute.
