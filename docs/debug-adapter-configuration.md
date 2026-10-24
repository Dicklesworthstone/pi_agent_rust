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

## Working directory and retained source paths

Pass `cwd` on launch or attach to select the local source directory. Relative
values resolve from the tool's workspace; an absolute directory is also
accepted. The directory must exist. Pi canonicalizes it before starting the
adapter or connecting to a loopback endpoint and retains it with that debugger.
Omitting `cwd` selects the tool's workspace, not another debugger's last root.

```json
{
  "action": "launch",
  "program": "main.py",
  "adapter": "debugpy",
  "cwd": "services/api",
  "initialBreakpoints": [{ "file": "main.py", "line": 12 }]
}
```

A later `set_breakpoint` or `remove_breakpoint` with `file: "main.py"` uses
that same `services/api` directory. Adding, removing and clearing a file's
breakpoints therefore operate on the initial set, even when a same-named file
exists in the workspace root. Absolute source paths still select their own
files. Missing source files retain the existing deferred-binding behavior:
their paths are normalized beneath the retained root, never replaced by a
same-named existing file in another directory.

The source root travels with the owned DAP session when conversations are
parked and restored. Failed startup does not publish a source root, and a fresh
launch does not inherit one from a terminated debugger. It appears as
`sourceRoot` in the startup `threadState`, `sessions[].execution`, and
`output.execution`. Changing `cwd` on a later breakpoint action is rejected;
start a new debugger to change its source base.

For local launch and PID attach, the adapter process also starts in the selected
directory and receives the existing local DAP cwd behavior. On loopback attach,
`cwd` selects **local source identity only**: it is not sent as a request to
change the remote process's working directory. `pathMappings.localRoot`,
`adapterConfig`, and relative adapter executable paths stay anchored to the
original tool workspace. Their configuration and containment rules do not change.

Canonicalizing the root prevents a retargeted cwd symlink from silently moving
this debugger to a different source tree. This is not a source-file snapshot or
a filesystem sandbox: descendants can change and existing source-path alias
normalization still applies. It adds no process, network or filesystem authority.

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

`src/debug/source_root_tests.rs` adds eight native-tool regressions for retained
launch/PID/port-attach roots, add/remove/clear wire requests, absolute and missing
source paths, parked conversation isolation, failed-start isolation, refusal
before a TCP connection, and Unix cwd-symlink retargeting. The deterministic
Python peer proves protocol routing only, not real-debugger breakpoint hits.
Missing Python is an explicit skip unless `PI_DEBUG_REQUIRE_PROTOCOL` is set.

These eight tests are authored, not executed. The required DSR command could not
start on the implementation host because `dsr` is absent (exit 127). Compilation,
Rust tests, formatting, Clippy, real-debugger and Windows acceptance remain
unverified. Existing tests and quality gates are unchanged.
