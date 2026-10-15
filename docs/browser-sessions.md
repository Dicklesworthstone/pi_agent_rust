# Conversation-owned browser sessions

The native `browser` tool now uses the same live conversation identity as jobs,
eval and debugger tools. A managed browser, its temporary profile, named tabs,
active-tab selection, snapshot references, upload copies and connection state
belong to one conversation. Switching conversations parks that state; returning
to the same retained identity restores it. A new or forked conversation starts
with a separate namespace and, when needed, a separate managed Chromium process.

This is implemented at the shared Tool layer. `ToolRegistry::from_tools`, normal
registry construction and later owned-tool mounting bind `JobSessionScope` via
`Tool::bind_job_session_scope`. Shared tool handles retain their original scope.
A standalone `BrowserTool::new` has an independent fixed identity. Hosts using
custom registries can bind a live scope through the same trait method. Return
the current nonempty identity; missing/blank/over-1024-byte identities fail
closed, never selecting a previous or process-global browser.

No caller-selected session-ID field is added. Tab names, target IDs and element
references are interpreted in the invocation's selected conversation. They are
not transferable handles: identical short names can exist in separate namespaces.
Keep each request and its response attached to the originating conversation.

## Retention and release

One tool retains at most **four namespaces**, without automatic eviction. At
capacity, a fifth namespace fails before backend dispatch. Switch to a retained
conversation to free its own slot:

```json
{"action":"stop"}
```

A successful native stop retains its existing confirmed owned-process stop
behavior and frees that conversation's slot. It still refuses to stop an
externally attached browser. A separate, idempotent release discards the selected
conversation's state without launching or contacting a browser:

```json
{"action":"release"}
```

Release accepts only `action` and optional `timeout_ms`. It drops the namespace
through the existing managed-process and upload cleanup guards. For a managed
browser this requests owned-process cleanup, rather than a CDP stop acknowledgement;
for an attached browser it only forgets Pi's mappings and upload staging. The
external process and its tabs stay open. Final tool drop and rebinding an owned
tool into a new registry also retire all of its old namespaces. Ordinary scope
resolver changes do not retire them.

Empty `status` and `stop` probes do not reserve a slot and do not launch a managed
browser, including at capacity. Other attempts reserve before dispatch. A failed
or cancelled attempt can already have adopted a browser or upload, so its slot
is conservatively retained until stop/release rather than treating an error as
proof of no side effects. This includes failed connection attempts. Repeating
work in that same conversation reuses its slot; a failed attempt never evicts
another conversation. Invalid release and invalid timeout inputs reserve nothing.

Successful results add `details.browserSession` with `scope: "session"`,
`reused`, `retained`, and `capacity`. Release reports `released` separately.
These fields never disclose other conversation IDs or contents. `reused` means
this namespace already existed, not that it was restored from durable storage.
Retention is in-process, not a session-log checkpoint or cross-process service.

## Invocation ownership and deadline

A call captures the live identity before waiting for a tool-wide asynchronous
execution lease and rechecks it after admission and before result publication.
Queued work never follows a later conversation switch. Backend effects always
belong to the original namespace, including effects completed before a mismatch
is detected. These boundary checks are not a live mid-operation identity watch,
an atomic lock with frontend switching, or rollback of browser/network effects.

The existing `timeout_ms` budget now also covers identity resolution and queue
wait, not just native startup/connection/action work. Its defaults remain 30,000
milliseconds, or 5,000 for `wait_for`, with an explicit range of 1–120,000. Quiet
waits check cancellation/deadline cooperatively every 20 milliseconds. Slow
identity resolvers are dropped under the original AgentCx when that budget ends.
A synchronous blocking poll cannot be preempted, but it is checked after returning
and cannot publish success after an observed cancellation/deadline.

## Attached-browser and workspace boundaries

**An explicitly attached CDP endpoint is still a shared external browser.**
Conversation-local Pi mappings do not isolate its cookies, storage, visible tabs
or raw target IDs. `list_tabs` still lists the endpoint's page targets and an
explicit target ID can select a page in that endpoint. Use managed browsers for
separate profiles; use attached mode only when the host intentionally chooses
that trusted shared browser. Release never kills an attached process.

The tool's workspace, domain policy and executable/endpoint settings remain
those chosen by its host. Changing conversation identity does not change cwd
or install software. Build separately rooted tools for separate workspaces;
release retained managed browsers before changing connection configuration.
Existing frame ancestry, dialog consent, diagnostics, file-transfer, export
and native deadline policies remain in the backend; this layer grants no new
execution or approval authority.

## Validation

Namespace routing, release, capacity and queue regressions are in
`src/browser/session/tests.rs`. Explicit mock cases test namespace plumbing, not
Chromium behavior; idle lifecycle cases exercise the native public path without
launching. Runtime acceptance requires the repository's DSR quality entrypoint:

```sh
dsr quality --tool pi_agent_rust
```

The implementation session had no `dsr` executable. Rust compilation, formatting,
Clippy and these authored tests were not executed; no full OMP parity is claimed.
