# MCP configuration and OMP imports

Pi discovers project MCP configurations from OMP, VS Code, Claude, Cursor,
Windsurf, Gemini, and Codex in the same registry as native Pi definitions.
Imported servers use the existing `/mcp` trust controls, executable identity
checks, transports, tool discovery, and resource/prompt access.

## Files and precedence

The first definition of a server name wins the entire definition; fields from
different files are never combined. Files are read in this order:

1. Explicit `--mcp-config <path>` files, in argument order.
2. Project `.pi/mcp.json`.
3. Project `.agents/mcp.json`.
4. Native user `~/.pi/agent/mcp.json` (or the configured agent directory).
5. Project `.omp/mcp.json`.
6. Project `.claude/mcp.json`, then `.claude/.mcp.json`.
7. Project `.cursor/mcp.json`.
8. Project `.windsurf/mcp.json`, then `.windsurf/mcp_config.json`.
9. Project `.gemini/settings.json`.
10. Project `.codex/config.toml`.
11. Project `.vscode/mcp.json`.
12. Project-root `mcp.json`, then `.mcp.json`.

Project files, including imports, are read only after workspace trust. An
explicit CLI file remains an operator-selected source. Foreign user-directory
files are not searched automatically; select one with `--mcp-config` when needed.
File-defined names also take precedence over later extension registrations.

JSON files normally contain a `mcpServers` object. Native and CLI files also
accept a bare server map. VS Code's `.vscode/mcp.json` accepts either `servers`
or `mcp.servers`; unrelated `inputs` are ignored. Codex imports the
`[mcp_servers.NAME]` TOML tables, including nested `env` and `http_headers` tables,
quoted names, booleans, comments, and multiline argument arrays.

Malformed higher-priority definitions reserve their names, preventing an older
file definition from silently becoming active. A whole-file failure in a native
or explicit CLI file stops discovery of lower-priority files. Warnings identify
the file and entry that need attention.

## One-shot operator commands

MCP setup and inspection can run without loading an LLM provider or creating an
agent session:

```bash
pi -p "/mcp list"
pi -p "/mcp inspect docs"
pi -p "/mcp trust docs"
pi -p "/mcp test docs"
pi -p "/mcp refresh docs"
pi -p "/mcp deny docs"
pi --mode json "/mcp refresh"
```

Use one quoted command argument; server names retain their exact internal
whitespace. `/mcp` is an alias for list. Refresh with a name reloads that server's
catalog; refresh without a name connects acknowledged servers within the shared
startup budget and reports all server health states. Inspect reports the
registry's provenance, trust, health, and source location without exposing raw
configuration credentials.

Workspace trust still controls project-file discovery. A server-specific trust
command acknowledges that exact definition and attempts to connect. If the
connection fails, an already-persisted trust decision remains in effect. Test
and refresh do not grant trust. Every one-shot command shuts down its temporary
connections before exiting.

Only standalone text from the original command-line arguments can invoke these
commands. A command mixed with another prompt, piped input, or file attachments
is rejected. File contents and expanded prompt templates cannot gain operator
authority.

JSON mode emits a `mcp_result` record with `command`, `success`, and either
`data` or `error`. Command failures return a nonzero process status.
For a refresh of all servers, inspect each returned server's health.

## Server settings

This OMP-compatible project configuration starts `./serve` in the workspace's
`services/docs` directory and allows up to two minutes per operation:

```json
{
  "mcpServers": {
    "docs": {
      "command": "./serve",
      "args": ["--index", "${workspaceFolder}/docs"],
      "cwd": "services/docs",
      "env": {
        "API_TOKEN": "${env:DOCS_TOKEN}",
        "MODE": "${DOCS_MODE:-local}"
      },
      "enabled": true,
      "timeout": 120000
    }
  }
}
```

`cwd` may be absolute or relative to the session workspace. Relative executable
paths, executable trust acknowledgments, process spawning, and `$CMD:` credential
helpers use this same directory. Changing the effective directory changes the
trust fingerprint. Pi does not change the host process's working directory.

## Protocol negotiation and workspace roots

Native stdio and Streamable HTTP connections request **MCP 2025-11-25**, the
revision used by OMP 18.12.6. Servers can select **2025-06-18** or **2025-03-26**
instead. Unsupported revisions and malformed initialization results stop the
connection before tool discovery or mounting. HTTP requests use the exact
negotiated `MCP-Protocol-Version`, including after session renewal and during
orderly shutdown.

The client advertises `roots: {"listChanged": false}` and answers `roots/list`
with one canonical `file://` URI for the session workspace. The root stays
fixed for that connection. A separate server `cwd` still controls process and
credential-helper execution; it does not change the workspace reported to the
server. Paths containing spaces, `#`, `%`, or non-ASCII characters are encoded
as file URIs. A server request cannot select additional roots or cause Pi to
read local files. Roots describe project scope; they do not sandbox the server
process or replace its existing trust and tool-approval controls.

The same handshake and root behavior applies to native MCP connections in
interactive, print, RPC, SDK, and ACP sessions. Server trust now binds both the
execution directory and the exposed workspace. A prior acknowledgment must be
renewed through the existing trust flow after this update; an acknowledgment
from another project cannot authorize a new root even when server `cwd` is fixed.

These connections advertise no sampling, elicitation, or task capabilities.
HTTP OAuth is available through the native authentication flow below.
Configured authentication headers and credential references remain supported.

Protocol references: [OMP v18.12.6 client](https://github.com/can1357/oh-my-pi/blob/v18.12.6/packages/coding-agent/src/mcp/client.ts),
[MCP lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle),
and [workspace roots](https://modelcontextprotocol.io/specification/2025-11-25/client/roots).

## Native HTTP OAuth

Trusted HTTP MCP servers can use a native browser sign-in flow through both
terminal interfaces, print commands, ACP operator commands, and RPC requests.
The client discovers the resource and authorization-server metadata, uses PKCE
with an S256 challenge, and receives the authorization response on a temporary
loopback listener.
OAuth endpoints must use HTTPS; loopback HTTP is accepted for local servers
and development.

Review the server's source configuration and acknowledge its trust before
starting authentication:

```text
/mcp trust docs
/mcp reauth docs
```

A protected server can reject the initial trust/connect attempt with an
authentication error; the trust acknowledgment remains saved. Run reauth next,
open the displayed sign-in URL in your browser, and complete the authorization.
Successful login saves the credentials, reconnects the server, and loads its
tool catalog. Cancelling the browser wait does not clear an existing login.
After credentials have been saved, a reconnect failure or late cancellation
does not roll them back. The browser flow has a five-minute outer deadline.

```text
/mcp unauth docs
```

Unauth removes the local managed login and retires the live connection and tool
catalog. It does not change trust or edit the server definition, and remains
available after a definition has been denied or disabled. It does not revoke
the account's grant at the remote authorization server.

Credentials live in the configured global Pi agent directory using the existing
private, atomic credential storage. They are bound to the exact server
definition, endpoint, workspace, and source identity. A changed definition
cannot inherit another server's login. Tokens are never copied into project MCP
configuration or editor-supplied definitions.

Access tokens are attached to every HTTP channel, including event streams,
cancellation notifications, and session termination. Near-expiry tokens can be
refreshed before an ordinary request. Refresh rotation is saved before the
replacement token is used; cleanup requests do not initiate refresh or browser
interaction. Logout invalidates older refresh and browser-flow commits, and old
connections cannot borrow credentials from a replacement login.

An explicit configured `Authorization` header remains authoritative. Managed
reauth/unauth refuse such definitions; remove that header from the source
configuration before switching to managed OAuth. Authentication failures appear
as authentication-required status and do not spend the server-crash restart
budget. A rejected tool call is not replayed. If authentication fails after a
request has already been accepted, the result remains outcome-unknown.

### Terminal authentication

Both terminal interfaces accept `/mcp reauth <name>` and `/mcp unauth <name>`.
The sign-in URL appears in the transcript; open it in a browser to continue.
Escape or Ctrl-C cancels the active authentication request. Login does not
submit a model turn, and new foreground input waits until the operation ends.

Authentication retains its own cancellation handle even when an earlier
command is still finishing. A delayed background message cannot release the
login's busy state, and a queued session change cannot redirect it to another
session. Login and logout reconcile the live tool catalog before releasing
the terminal for further work.

### One-shot authentication

```bash
pi -p "/mcp reauth docs"
pi --mode json "/mcp unauth docs"
```

These commands run before LLM credential loading, resource expansion, extension
startup, and model selection. They use existing server-trust acknowledgments;
complete the trust step with `pi -p "/mcp trust docs"` or in an interactive/ACP
session first. They accept one standalone quoted command argument. Piped text,
file attachments, additional prompts, and expanded prompt templates cannot
authorize this operation.

JSON mode emits a `mcp_auth_url` record with the server and sign-in URL, followed
by a `mcp_auth_result` record containing the command, server, and success status.
Failures return a nonzero process status. Successful one-shot login closes its
temporary MCP connection while retaining the saved credentials for future
sessions.

### ACP and RPC authentication

ACP accepts `/mcp reauth <name>` and `/mcp unauth <name>` as standalone
operator commands. Sign-in URLs are delivered through the active session's
updates. The request remains cancellable, and the resulting tool catalog is
reconciled before the operator request finishes. These commands do not ask a
model to interpret the command.

RPC exposes explicit requests with exact server names:

```json
{"id":"login-docs","type":"mcp_reauth","server":"docs"}
{"id":"logout-docs","type":"mcp_unauth","server":"docs"}
```

A browser sign-in first emits a `mcp_auth_url` event containing the original
request `id`, the `command`, `server`, and `url`. Completion uses the ordinary
RPC response envelope, with `command: "mcp_reauth"` or `"mcp_unauth"`, a
`success` flag, and either an error or `data: {"server":"docs","authenticated":true}`
(`authenticated` is `false` for successful logout).

Authentication requires an idle RPC session. Other session-changing requests
are rejected as busy until it finishes; the normal `abort` request can cancel
it. The browser
wait does not hold the session mutex and does not emit a model turn or append
conversation entries. A failed URL delivery ends the login attempt.

By default, browser sign-in uses the authorization server's dynamic client
registration endpoint. If the server requires a previously registered public
client, provide its identifier through RPC:

```json
{"id":"login-docs","type":"mcp_reauth","server":"docs","clientId":"registered-public-client"}
```

The manager API also accepts this optional client ID. Terminal, print, and ACP
commands use dynamic registration by default. Custom callback ports, scopes, and OMP
OAuth configuration fields are not imported by this native flow.

## Request timeouts

`timeout` is an integer in milliseconds, from **1 to 3,600,000**; `timeoutMs` and
`timeout_ms` are accepted aliases. Use only one spelling. Missing timeouts retain
the native **30-second** default. The value applies to initialization, tool
discovery, tool calls, and resource/prompt/completion requests. Pagination shares
one operation budget. Session cancellation, caller deadlines, and the existing
eight-second aggregate startup budget can end work earlier. Zero, negative,
fractional, null, and over-limit values are rejected explicitly; OMP's zero for
an unlimited timeout is intentionally not imported as unlimited work.

## Disabled servers

`enabled: false` or `disabled: true` keeps a server disabled, even if it has an
existing trust acknowledgment. Disabled definitions remain visible in `/mcp`
with health `disabled`, occupy their names, and do not resolve credentials,
spawn, connect, mount tools, or expose context tools. A disable-only entry such
as `{"disabled": true}` can suppress a lower-priority definition.

Native user and explicit CLI files can additionally contain:

```json
{
  "disabledServers": ["legacy", "extension-server"],
  "enabledServers": ["docs"],
  "mcpServers": {}
}
```

`disabledServers` wins over all enable flags, including `enabledServers`, and
also blocks later extension registrations with those names. `enabledServers`
can override a discovered `enabled: false`; it cannot override `disabled: true`.
Both lists must be arrays of valid server names. Only explicit CLI and native
user files have this cross-source override authority. Project/imported files
use each server's own `enabled` or `disabled` setting.

Enabling a server does not acknowledge trust. Edit its configuration, restart
the session, and use the existing trust flow for a newly enabled definition.
`/mcp trust` refuses a server that is still disabled.

## Variables and credentials

String configuration values support `${VAR}`, `${VAR:-default}`, `${env:VAR}`,
and `${workspaceFolder}`. Missing or empty environment variables require a
default. `${workspaceFolder}` always refers to the session workspace, including
when the server selects a different `cwd`. VS Code `${input:...}` prompts are
not evaluated; replace them with environment references.

Command, arguments, URL, and `cwd` expand during discovery so their resolved
execution targets are included in trust review. Environment and HTTP-header
templates remain unresolved until the trusted transport-construction boundary.
The existing `$ENV:NAME` and `$CMD:command` credential references remain
supported. Expanded variable values are treated as literal data and are never
reinterpreted as credential commands. ACP client-supplied env/header values
remain literal and are not interpolated.

Compatibility references: [OMP v18.12.4 MCP schema](https://github.com/can1357/oh-my-pi/blob/v18.12.4/packages/coding-agent/src/config/mcp-schema.json),
[discovery normalization](https://github.com/can1357/oh-my-pi/blob/v18.12.4/packages/coding-agent/src/discovery/mcp-json.ts),
and [VS Code import fixtures](https://github.com/can1357/oh-my-pi/blob/v18.12.4/packages/coding-agent/test/discovery/vscode-mcp.test.ts).
