# Configured native debug adapters

Pi can launch a named stdio DAP adapter from an explicitly selected workspace
`dap.json`. This extends the same `debug` tool, not a separate debugger or a
shell-based substitute. Existing session scoping, retained breakpoints, opaque
inspection handles, cancellation and process-tree cleanup still apply.

## Use a project Python environment

Create `.pi/dap.json`:

```json
{
  "adapters": {
    "debugpy": {
      "command": "./.venv/bin/python",
      "launchDefaults": { "justMyCode": false },
      "attachDefaults": { "logToFile": false }
    }
  }
}
```

Then call `debug`:

```json
{
  "action": "launch",
  "program": "main.py",
  "adapter": "debugpy",
  "adapterConfig": ".pi/dap.json",
  "stopOnEntry": true,
  "initialBreakpoints": [{ "file": "main.py", "line": 12 }]
}
```

Omitted `args` inherit the registered debugpy adapter arguments (`-m
 debugpy.adapter`); supplying `args: []` deliberately clears them. Adapter
arguments are distinct from the tool's `args`, which go to the debuggee.
Existing registered IDs also preserve their native launch/attach profiles.
A host's `with_adapters` definitions participate in this inheritance.

For local process attachment, use `action: "attach"` and `pid` with the same
explicit `adapter` and `adapterConfig`. `attachDefaults` are separate from
`launchDefaults`. A loopback `port` attachment never starts a local adapter,
so combining it with `adapterConfig` is rejected.

## Add a stdio backend

```json
{
  "adapters": {
    "my-adapter": {
      "command": "/opt/tools/my-dap-adapter",
      "args": ["--stdio"],
      "connectMode": "stdio",
      "acceptsDirectoryProgram": false,
      "launchDefaults": { "adapterSpecificSetting": true }
    }
  }
}
```

Choose `adapter: "my-adapter"` and this file explicitly. A new ID requires a
`command`; built-in/SDK-registered IDs may override only their defaults. Bare
commands resolve on PATH; relative commands containing a path separator
resolve against the debugger workspace, not the configuration file's parent.
The resolved executable uses the existing absolute-path process launcher.
Arguments are literal argv elements: spaces, dollar signs and empty arguments
are preserved. There is no shell, environment interpolation, template expansion,
package installation or download step.

`acceptsDirectoryProgram: true` admits a package-directory target for an
adapter that actually understands it. Delve retains its existing directory
support, Go mode checks, managed build artifacts and owned loopback transport.
Other configured adapters use stdio. Socket and generic TCP adapters are
explicitly unsupported, not silently launched with the wrong transport.

The supported fields use OMP's `command`, `args`, `launchDefaults`,
`attachDefaults`, `connectMode` and `acceptsDirectoryProgram` spellings.
`languages`, `fileTypes` and `rootMarkers` are accepted as bounded metadata;
they do not auto-select or auto-execute a workspace adapter. This is not a
claim of complete OMP configuration discovery or debugger-backend parity.

## Authority and limits

A file's presence grants no execution authority. Both `adapterConfig` and the
adapter ID must appear on an admitted local launch/attach tool call. Treat the
selected definition as executable configuration and review its command,
arguments and defaults before using it. It is read afresh at startup; changing
the file does not reconfigure an already-running debugger. This is a cooperative
workspace boundary, not a content-addressed approval receipt or protection
against a hostile writer racing filesystem checks.

Paths must be workspace-relative without parent traversal. A resolved path
outside the workspace, non-regular file, file over 64 KiB, invalid JSON, unknown
field, invalid ID, missing selected definition or unsupported transport is an
error. There is no silent fallback to another adapter after an explicit
configuration fails. At most 64 definitions and 128 argv elements are accepted.
Configuration parse errors do not echo values that could contain credentials.

Defaults can add adapter-specific settings but cannot replace caller-controlled
`program`, `args`, `cwd`, `pid`, `processId`, execution `mode`, managed `output`,
stop policy or host-controlled terminal/subprocess fields. Native arguments win
when merging. Configured adapters do not gain new reverse-request authority;
only the existing explicit local debugpy/LLDB terminal path can service
`runInTerminal`.

## Validation

`src/debug/configuration.rs` includes regressions for explicit selection,
registered-argument inheritance, distinct launch/attach defaults, literal argv,
directory opt-in, traversal/symlink rejection, unsupported transports, protected
fields, size limits and error redaction. These are authored native tests, not a
claim that they ran on the implementation host. The required authoritative
validation is `dsr quality --tool pi_agent_rust`.
