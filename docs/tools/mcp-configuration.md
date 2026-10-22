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
OAuth login and `/mcp reauth` are not implemented by this protocol update;
configured authentication headers and credential references retain their
existing behavior.

Protocol references: [OMP v18.12.6 client](https://github.com/can1357/oh-my-pi/blob/v18.12.6/packages/coding-agent/src/mcp/client.ts),
[MCP lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle),
and [workspace roots](https://modelcontextprotocol.io/specification/2025-11-25/client/roots).

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
