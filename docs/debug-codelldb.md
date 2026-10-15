# CodeLLDB native debugging

The native `debug` tool supports CodeLLDB 1.12.3's framed **stdio** mode. No
listening port, port discovery, reverse-connect handshake or second DAP client
is required. The normal owned process transport and conversation-scoped session
provide launch/attach, initial breakpoints, stepping, inspection handles and
cleanup. This extends the OMP-inspired debugger track, not a full OMP parity claim.

```json
{"action":"launch","program":"target/debug/my-app","adapter":"codelldb","initialBreakpoints":[{"file":"src/main.rs","line":12}]}
```

```json
{"action":"evaluate","expression":"value","context":"watch"}
```

```json
{"action":"attach","pid":12345,"adapter":"codelldb"}
```

Use `disconnect` to preserve an attached target; `terminate` requests termination
through the existing session policy. Refetch frame/object handles after resuming.
CodeLLDB's `terminal:"console"` keeps target output on the DAP channel; Pi does
not grant CodeLLDB a `runInTerminal` host. `runInTerminal:true` is rejected before
spawn. Interactive target stdin through `terminal_input` is not supported for
this backend. Existing debugpy/LLDB terminal behavior is unchanged.

## Installation and selection

Install a trusted CodeLLDB 1.12.3 distribution with its associated LLDB libraries.
Put `codelldb` (or `codelldb.exe`) on PATH, or explicitly select its absolute
adapter executable in a workspace `dap.json`:

```json
{"adapters":{"codelldb":{"command":"/opt/codelldb/adapter/codelldb","connectMode":"stdio"}}}
```

Then add `"adapterConfig":".pi/dap.json"` and `"adapter":"codelldb"` to the
launch/attach call. Configuration is not auto-loaded. A command-only override
inherits the native stdio profile. Source mapping and other supported
adapter-specific defaults can be supplied via `launchDefaults`/`attachDefaults`.
Relative executable paths remain relative to Pi's workspace, not the config file.

Native auto-selection preserves the previous LLDB then GDB preference and tries
CodeLLDB afterwards. Explicit IDs and SDK overrides retain priority. There is no
implicit package installation, VS Code directory scan or live probe during
selection. An incompatible installed executable fails the ordinary initialize
exchange; Pi does not silently fall back after a selected adapter fails.

Unlike OMP's older `--port 0` profile, this integration requires native stdio
support. CodeLLDB `adapterConfig` definitions reject `--port`, `--connect`,
`--multi-session`, their `=value` forms and the `--` argument terminator before
spawn, even when `connectMode` says `stdio`. Those arguments would otherwise
start an incompatible transport or lifecycle while Pi reads the stdio pipes.
Literal `--liblldb` arguments remain supported. Unrelated custom adapter IDs
and trusted SDK `with_adapters` definitions are unchanged; SDK hosts remain
responsible for supplying argv matching the native transport.

Adapter arguments replace the default argv and are executable configuration,
not a sandbox. CodeLLDB and its target retain their normal process authority
and ambient environment.

## Validation

Seven tests in `src/debug/codelldb_tests.rs` cover the native profile, five
public-tool protocol workflows and one opt-in actual CodeLLDB workflow. Two
additional configuration tests cover transport-argument rejection, command-only
inheritance and preservation of unrelated custom definitions. The public
rejection case checks that no adapter process starts and no session is retained.
The protocol peer is deterministic Python, not an LLDB implementation. The live
lane compiles a tiny C program, hits a source breakpoint, inspects 40, steps to
42 and requests termination. A DSR host needs `cc`, CodeLLDB and debugging
permissions to run it:

```sh
PI_DEBUG_REQUIRE_CODELLDB=1 PI_DEBUG_REQUIRE_PROTOCOL=1 dsr quality --tool pi_agent_rust
```

The live test explicitly reports a skip without its opt-in. The implementation
host could not start DSR (`dsr` absent, exit 127); no Rust compilation, formatting,
Clippy, native protocol tests or actual CodeLLDB test was executed here.

The upstream protocol reference is CodeLLDB v1.12.3, commit
`62def434dc22c1d77d4837c4d99cddef7ac3338f`, particularly
`src/codelldb/src/lib.rs` (stdio default) and
`src/codelldb/src/debug_session/launch.rs` (console and launch/attach behavior).
