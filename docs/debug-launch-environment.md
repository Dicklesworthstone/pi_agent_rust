# Per-launch debugger environment

The native `debug` tool accepts an explicit `env` object on `action: "launch"`:

```json
{
  "action": "launch",
  "program": "app.py",
  "adapter": "debugpy",
  "cwd": "service",
  "env": {
    "APP_ENV": "development",
    "FEATURE_FLAG": "enabled",
    "EMPTY_VALUE": ""
  }
}
```

The variables are added to the DAP launch request as `env`. Pi does not mutate
its process environment, pass these overrides to the adapter's own spawn, or
store them as defaults for a later debugger session. Target inheritance and
application of launch variables remain the selected adapter's responsibility.
An adapter that does not implement this launch field may reject or ignore it;
this feature is not a claim of tested environment support for every backend.
The existing explicit local terminal-launch path still owns reverse requests.

This adds a native per-call convenience to the existing OMP-inspired adapter
configuration workflow. It is not a claim that OMP v18.12.4 exposes this exact
per-call field: that version exposes adapter `launchDefaults` customization.

## Defaults and literal values

When `adapterConfig` supplies an object-shaped `launchDefaults.env`, the tool
merges it with the explicit object. Explicit entries win; unrelated configured
entries survive. On Windows, the portable names compare case-insensitively and
an explicit spelling replaces the configured spelling. Ambiguous duplicate
spellings within either map are refused. Other platforms keep case-sensitive
names. Empty strings set an empty value; they do not delete a variable.

There is no shell expansion, variable interpolation, dotenv loading, file read
or implicit credential lookup for values. `$HOME`, `${NAME}`, whitespace,
newlines and Unicode values are forwarded literally. Supplying `env: {}` keeps
compatible configured variables; omitting `env` leaves all existing adapter
configuration behavior unchanged, including adapter-specific representations.
Explicit `null`, arrays, numbers, booleans and object-valued entries are errors,
not absence or conversion to strings. Unsetting variables is not supported by
this portable per-call field. Continue to use an explicitly selected adapter
configuration for backend-specific representations.

## Admission and limits

`env` is launch-only. PID or port attachment cannot change an already-running
process's environment, so the tool rejects it on attach and every other action,
including an empty object. These refusals happen before adapter spawn or network
connection. A malformed configured map combined with explicit overrides is also
rejected before spawn; it does not silently fall back to a different environment.

Names use `[A-Za-z_][A-Za-z0-9_]*` and may occupy at most 256 bytes. Each map and
the merged result are limited to 128 entries and 64 KiB of UTF-8 name/value
bytes plus two separator bytes per entry. Values cannot contain NUL. Validation
and merged-limit failures leave the launch arguments unchanged. No new runtime,
process authority, permission bypass, dependency or automatic installation is
introduced.

New validation errors, the environment's Rust debug representation and normal
launch receipts do not include names or values. This is not a secret boundary
against the chosen debugger, target, tool-input logging or a deliberate
inspection expression: those can observe or echo the target environment. Treat
both the selected adapter and debuggee as executable code.

## Validation

Seven unit tests exercise presence versus null, literal/default merging, byte
and count limits, invalid value redaction, action scope, schema and platform
case rules. Three Unix protocol tests invoke the real `DebugTool::execute`
path against the repository's deterministic Python DAP peer, checking actual
wire arguments, unchanged adapter/host environment, fresh-launch isolation,
refusal before spawn, and recovery after invalid configuration. These fixture
cases do not execute a debuggee or prove backend interoperability.

A separate opt-in test launches actual debugpy and evaluates a literal variable
in its Python target. Enable that lane, together with required fixture Python,
on the authoritative DSR host:

```sh
PI_DEBUG_REQUIRE_DEBUGPY_ENV=1 PI_DEBUG_REQUIRE_PROTOCOL=1 dsr quality --tool pi_agent_rust
```

All eleven Rust tests are authored, not executed on the implementation host.
The required DSR command was attempted but could not start (`dsr` missing,
exit 127). Compilation, formatting, Clippy, live-debugger, PTY and Windows
acceptance remain unverified. Existing debugger tests were not weakened.
