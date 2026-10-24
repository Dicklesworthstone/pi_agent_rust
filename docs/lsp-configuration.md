# Language-server workspace settings

The native LSP client answers `workspace/configuration` and
`workspace/workspaceFolders` requests, including requests made while its
initialization is pending. These services remain installed alongside the
selected-code-action edit handler. They never open an edit permission window.

## Persist a server's settings across sessions

Add an explicit `settings` object to that server in global `settings.json` or a
trusted project's `.pi/settings.json`. For example:

```json
{
  "lsp": {
    "servers": {
      "gopls": {
        "settings": {
          "gopls": {
            "gofumpt": true
          }
        }
      }
    }
  }
}
```

Use the setting names documented by the selected language server. This object
is separate from `initializationOptions`: initialization-only options go in the
initialize request; workspace settings answer `workspace/configuration` and are
sent in `workspace/didChangeConfiguration` after `initialized`.

The configuration loader merges server entries by name and field. A project
override containing only `settings` keeps a globally configured server's command,
arguments, environment, languages, extensions and root markers. Each supplied
object-valued field replaces that whole field, without a deep merge. To reset
inherited settings explicitly, use `"settings": {}`.

Only the settings passed through the existing configuration loader are used.
Untrusted project settings remain excluded; global configuration and an explicit
configuration-file override retain their existing behavior. This does not add
another configuration-file search or executable-discovery path.

Before spawning a server, Pi validates the settings object's size and shape
using the same limits as the `configure` action. Reverse configuration requests
during initialization see that object immediately. The initial notification is
queued after `initialized` and before the connection becomes available to tool
operations. Initialization and initial notification admission share one startup
deadline. Failure does not publish a ready registry entry.

An explicitly configured empty object is notified once. If settings are absent,
the connection starts with an empty response snapshot and no initial notification.
Queue acceptance does not establish that the server has adopted the settings or
finished analyzing the workspace.

## Inspect, preview, and explicitly replace settings

Choose one server/root using an existing regular anchor file:

```json
{"action":"configure","file":"src/lib.rs","timeout":10}
```

The response contains that connection's settings, local revision, and
`configured` status. A connection without configured settings exposes `{}` at
revision zero. A configured startup snapshot begins at revision one after its
initial notification is accepted. Inspection does not send another notification.
Unrelated Pi configuration, credentials, environment and initialization-only
options are never substituted for the explicitly supplied workspace settings.

Supply an object in `payload` to preview a full replacement:

```json
{"action":"configure","file":"src/lib.rs","payload":{"rust-analyzer":{"cargo":{"allFeatures":true}}}}
```

Repeat the payload with `apply:true` to accept it:

```json
{"action":"configure","file":"src/lib.rs","payload":{"rust-analyzer":{"cargo":{"allFeatures":true}}},"apply":true}
```

The section names and values must match the chosen server's documented settings.
This is not a merge: omitted keys disappear. An explicit first application of
`{}` is sent, allowing a server to reset to defaults. Repeating already configured
identical settings is a no-op, without a notification or revision increment.
An explicit null, scalar, or array payload is invalid, not a request to inspect.

Preview does not change settings or retire existing refactor handles. It may
start the server so it can identify the connection, but it does not synchronize
or read the anchor's text. A preview is not a stored approval handle; application
uses the payload explicitly supplied on that call, not a prior preview.

`notificationQueued:true` means the complete `workspace/didChangeConfiguration`
notification was accepted by the transport, **not** that the language server
acknowledged, honored, or finished analyzing the new settings. The protocol
notification has no response. The local revision is not a server revision.
A failed queue admission does not publish the new settings or advance that
revision; the transport may be retired and prior document evidence invalidated.

## Lifetime, isolation, and evidence

A replacement made through `configure` belongs to one live server connection.
The tool does not write it into `settings.json` or persist it in the session.
Reload, idle retirement, or process replacement restores that server's configured
startup settings, or `{}` when none were supplied. Other server/root connections
retain their independent values. Reload reuses the configuration loaded for this
tool instance; changes to `settings.json` take effect in a new Pi session.

A changed application retires synchronized document incarnations, visible
diagnostic reports, and cached action, completion, hierarchy, and reviewed
refactor handles. Older edits cannot be approved under different analysis
settings. A subsequent diagnostic pull cannot reuse a result ID bound to an
older document version. Preview and no-op application do not retire those
handles. This does not promise that a server's unversioned diagnostic pushes
prove fresh analysis or that all project dependencies form an atomic snapshot.

The caller needs filesystem I/O authority before anchor inspection or server
startup. One request budget covers local admission and startup after the tool
workflow lane is acquired; synchronous filesystem calls cannot be preempted.
Once a configuration notification is accepted, the operation reports that effect
rather than describing a later cancellation as proof nothing was sent.

## Server requests

`workspace/configuration` returns one value for each requested item, in the
original order, including duplicates. `section` uses dot-separated nested object
keys. Missing sections return null; false, zero, empty strings, arrays, and null
values retain their JSON types. An absent, null, or empty section requests the
whole explicitly configured object. Flat dotted keys are not expanded.

An omitted or null `scopeUri` selects this connection's settings. Local file URIs
lexically within its root receive the same settings; there are no per-subfolder
overrides. Valid foreign-root and non-file URIs receive null. URI paths are never
opened, fetched, or resolved through the filesystem for this lookup. Malformed
requests receive a JSON-RPC invalid-params error, not a successful null response.
Outstanding requests can describe an earlier immutable settings snapshot;
this is not a server-side configuration barrier.

`workspace/workspaceFolders` returns the same one-root array supplied during
initialization. This does not add multiple folders to a single server connection.
Dynamic configuration registration is not advertised or implemented.

Settings and configuration requests are bounded at 64 KiB of serialized JSON,
32 nesting levels, and 8,192 values. Requests allow at most 128 items; sections
are at most 1,024 bytes and scope URIs at most 8,192 bytes. Replies are bounded at
256 KiB before cloning repeated section values. Oversized or malformed requests
fail as a whole, without silently dropping items. No settings are included in
error messages; successful inspection intentionally returns the supplied values.

A language server can itself act on settings (including server-specific build
commands or paths). This feature does not sandbox that process or validate the
meaning of arbitrary server options. It grants no extra host command execution
or unsolicited `workspace/applyEdit` authority and does not expand edit scope.
