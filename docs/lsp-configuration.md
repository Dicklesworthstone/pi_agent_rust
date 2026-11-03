# Language servers: setup and configuration

[Documentation home](README.md) · [Settings reference](settings.md)

**For Pi users:** start with installation and setup below.
**For agents and integrators:** see the [tool and protocol reference](#tool-and-protocol-reference).

Pi uses language servers for diagnostics, symbol navigation, completion, and
refactoring. It includes **five server definitions**, but does **not install the
server programs**. With the default tool selection, an installed server on
`PATH` needs no `settings.json` entry for the file types below.

The five defaults and executable overrides described here are available in
`v0.8.0`. The later workspace-settings reference has a separate version note.

## Quick start

1. Install the server for your language from the table below.
2. Start `pi` from your project directory, using the same environment in which
   the server executable is available.
3. Ask Pi: **“Use the LSP tool to check `src/main.rs` for diagnostics.”**
   Replace `src/main.rs` with an existing source file in your project.

`lsp` is an **agent tool**, not a built-in `/lsp` slash command or `pi lsp` shell
subcommand. You request it in ordinary language. With the default tool selection,
the agent discovers and calls it through `xdev`; it does not need an extension.
Explicit `--tools`, `--no-tools`, or `tools.loadMode` settings can change tool
availability.

## Built-in server definitions

Run installation commands in your shell. The relevant toolchain or package
manager must already be installed.

| Language and file extensions | Definition key | Executable Pi starts | Installation |
| --- | --- | --- | --- |
| Rust: `.rs` | `rust-analyzer` | `rust-analyzer` | `rustup component add rust-analyzer` |
| JavaScript / TypeScript: `.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.cjs` | `typescript-language-server` | `typescript-language-server --stdio` | `npm install -g typescript-language-server typescript` |
| Python: `.py`, `.pyi` | `pyright` | `pyright-langserver --stdio` | `npm install -g pyright` |
| Go: `.go` | `gopls` | `gopls` | `go install golang.org/x/tools/gopls@latest` |
| C / C++: `.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hpp` | `clangd` | `clangd` | Debian/Ubuntu: `sudo apt install clangd`; macOS/Homebrew: `brew install llvm` |

Pi supplies the arguments shown in the executable column; you do not need to
launch these processes yourself. A server installed only inside an editor
extension may not be available to Pi.

Check the executable from the shell you use to launch Pi, for example:

```sh
command -v pyright-langserver
```

On PowerShell, use `Get-Command pyright-langserver`. For Go, ensure the install
directory (`GOBIN`, or normally `GOPATH/bin`) is on `PATH`. Homebrew LLVM's `bin`
directory may also need to be added to `PATH`. Alternatively, configure an
absolute executable path as shown below.

For platform options, dependencies, and compatible versions, see the upstream
instructions for [rust-analyzer](https://rust-analyzer.github.io/book/rust_analyzer_binary.html),
[TypeScript Language Server](https://github.com/typescript-language-server/typescript-language-server#installing),
[Pyright](https://github.com/microsoft/pyright/blob/main/docs/installation.md),
[gopls](https://go.dev/gopls/#installation), and
[clangd](https://clangd.llvm.org/installation.html).

## What Pi detects automatically

When the agent requests an LSP operation for a source file, Pi:

1. Selects a configured server by the file's extension.
2. Walks upward from that file to the nearest directory containing any root
   marker for that server.
3. Falls back to Pi's working directory if no marker is found.
4. Resolves the executable by its configured path or through `PATH`, starts it when needed,
   and reuses the connection for that server and workspace root.

| Server | Workspace-root markers |
| --- | --- |
| `rust-analyzer` | `Cargo.toml`, `.git` |
| `typescript-language-server` | `package.json`, `tsconfig.json` |
| `pyright` | `pyproject.toml`, `setup.py`, `requirements.txt` |
| `gopls` | `go.mod`, `go.work` |
| `clangd` | `compile_commands.json`, `compile_flags.txt`, `.git` |

Pi selects and starts servers lazily. It does not install them or scan for every
language server at startup. Ask Pi **“Show the configured and running LSP
servers”** to inspect status. Configured entries are definitions; they do not
prove that the executable is installed. An empty list of live servers is normal
before a file-specific operation.

To check a file, request diagnostics explicitly. For a group of files, ask for
an active `workspace_diagnostics` scan. The `diagnostics` action with a glob
only reads reports already cached; it does not check previously unopened files.
See [active workspace diagnostics](lsp-workspace-diagnostics.md).

## Override a built-in server

Add configuration to `~/.pi/agent/settings.json` for all projects, or to a trusted
project's `.pi/settings.json`. See [settings locations and precedence](settings.md).

For example, use a specific rust-analyzer executable:

```json
{
  "lsp": {
    "servers": {
      "rust-analyzer": {
        "command": "/absolute/path/to/rust-analyzer"
      }
    }
  }
}
```

Replace the example path with your installed executable. `command` is one
executable name or path; put command-line arguments in an `args` array rather
than inside `command`.

An entry with a built-in name overrides only the fields you supply. The example
keeps rust-analyzer's default extensions and root markers. Supplied arrays and
objects replace that entire field, so include every argument, extension, or
setting you want to retain. To replace a built-in server for its existing file
types, override its existing definition key.

## Add another language server

For example, install [Bash Language Server](https://github.com/bash-lsp/bash-language-server):

```sh
npm install -g bash-language-server
```

Then add:

```json
{
  "lsp": {
    "servers": {
      "bash-language-server": {
        "command": "bash-language-server",
        "args": ["start"],
        "languages": ["shellscript"],
        "extensions": [".sh", ".bash"],
        "rootMarkers": [".git"]
      }
    }
  }
}
```

A new definition needs `command` and matching `extensions` to serve files.
Use lowercase extensions with leading dots. Supported operations depend on
the server's capabilities.

Quit and restart Pi after editing `settings.json`. The LSP `reload` action
restarts server processes using the configuration already loaded for that tool
instance; it does not reread the settings files.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| `LSP_SERVER_MISSING` | Read the spawn error and installation hint. Check the executable name, `PATH`, and execute permissions; use an absolute `command` path if needed. |
| `LSP_NO_SERVER` | Check the file extension against the table, or add a matching `lsp.servers` definition. |
| No live servers in status | Request diagnostics or capabilities for an existing source file; status alone does not start a server. |
| Project configuration has no effect | Confirm that the project is trusted and restart Pi after editing settings. |
| An operation returns `LSP_UNSUPPORTED` | Ask Pi to inspect the server's capabilities; individual servers support different LSP operations. |

## Tool and protocol reference

The JSON requests in the following sections are inputs to the agent's `lsp`
tool. They are not terminal commands or text to paste into Pi's slash-command
prompt. For example, the default `xdev` dispatcher can invoke LSP diagnostics
with this tool input:

```json
{"action":"run","name":"lsp","args":{"action":"diagnostics","file":"src/main.rs"}}
```

Related references: [navigation](lsp-navigation.md),
[completion](lsp-completion.md), [formatting](lsp-formatting.md),
[refactor previews](lsp-refactor-previews.md),
[project diagnostics](lsp-project-diagnostics.md), and
[active workspace diagnostics](lsp-workspace-diagnostics.md).

**Version note:** the per-server `settings` field and the `configure` action
in the following sections were added on `main` after the published `v0.8.0`
release. They are not included in that release.

The native LSP client answers `workspace/configuration` and
`workspace/workspaceFolders` requests, including requests made while its
initialization is pending. These services remain installed alongside the
selected-code-action edit handler. They never open an edit permission window.

### Persist a server's settings across sessions

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

### Inspect, preview, and explicitly replace settings

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

### Lifetime, isolation, and evidence

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

### Server requests

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
