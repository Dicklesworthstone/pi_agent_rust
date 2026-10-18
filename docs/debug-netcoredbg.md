# Debugging built .NET applications

The native `debug` tool recognizes managed PE images by their CLR header and
metadata, rather than assuming every `.dll` is managed. A built application DLL
can select an installed `netcoredbg` automatically, including on Linux/macOS
where .NET application assemblies still use PE. Explicit adapter IDs and SDK
adapter definitions retain precedence.

```json
{"action":"launch","program":"bin/Debug/net8.0/Application.dll","adapter":"netcoredbg","stopOnEntry":true,"initialBreakpoints":[{"file":"Program.cs","line":12}]}
```

Build the application yourself first and retain its runtime configuration,
dependencies and debug symbols. Pi does not run a build, restore packages,
choose a target framework, install the debugger or certify runtime compatibility.
`netcoredbg` and the appropriate .NET runtime must be installed. Launch an
application output, not an arbitrary class library. For a native .NET apphost
(including ELF/Mach-O and single-file applications), select `adapter:netcoredbg`
explicitly; the managed-PE detector does not recognize native apphosts.

C#/F# source files and project/solution inputs selected with netcoredbg produce
`DAP_BUILD_REQUIRED` before adapter spawn. This includes explicitly configured
and SDK-registered netcoredbg definitions. Other backend IDs are unchanged.
A bare language classification is not a claim that source code is executable.

## Entry stop and attach

Pi's `stopOnEntry` input maps to netcoredbg's `stopAtEntry` launch argument.
The default is true. Explicit false clears that default rather than adding an
ignored `stopOnEntry` field. Adapter configuration cannot set a competing
`stopAtEntry` default. Other settings, such as `justMyCode`, remain available
through the existing explicit `adapterConfig` launch/attach defaults.

```json
{"action":"attach","adapter":"netcoredbg","pid":12345}
```

Attachment sends netcoredbg's `processId`, not the native LLDB/GDB `pid` field.
Use `disconnect` to preserve an attached target. Use `terminate` for a program
launched by Pi. The existing DAP capability checks, process ownership,
conversation isolation, breakpoint sequencing and expiring inspection handles
remain authoritative; this integration creates no second debugger runtime.

## Recognition bounds and limitations

Recognition checks DOS and PE signatures, PE32/PE32+ optional-header bounds,
the CLR directory, a file-backed CLI header and a `BSJB` metadata signature.
It permits at most 96 section headers, a 4096-byte optional header and a PE
header offset within the first MiB. It reads fewer than 9 KiB of fixed headers
without reading whole binaries or executing file/objdump/dotnet probes.
Overflow, truncation, missing headers, virtual-only data and ambiguous section
mappings do not identify a managed image. Native DLLs retain native routing.

This is conservative identification, not full PE validation, malware screening,
a .NET Framework/Core compatibility check, or a guarantee of an entry point.
File checks assume a cooperative workspace, not protection from hostile writers
racing filesystem metadata/open operations. Explicit adapter selection remains
available for unusual images outside the recognition bounds.

## Validation status

Eleven authored Rust tests cover PE32/PE32+ recognition, native DLL exclusion,
truncation and malformed mappings, read bounds, selection precedence, field
projection and build preflight. Four of those tests drive the public DebugTool
through actual stdio DAP framing to the existing deterministic Python peer,
checking startup ordering, inspection/stepping, attach/detach and configuration
refusal. The synthetic images are structural fixtures, not runnable assemblies;
the peer is not netcoredbg. These tests do not establish live .NET acceptance.

The implementation host attempted `dsr quality --tool pi_agent_rust` but DSR
was absent (exit 127). Compilation, formatting, Clippy and all Rust tests remain
unexecuted. Run the repository's DSR recipe and a real netcoredbg application
workflow before claiming tested interoperability. `PI_DEBUG_REQUIRE_PROTOCOL=1`
makes the native protocol cases fail rather than skip when Python is missing.

Protocol references: Microsoft PE/COFF image specification (CLR directory and
section mapping); Samsung/netcoredbg `src/protocols/vscodeprotocol.cpp` and
`test-suite/NetcoreDbgTest/VSCode/VSCodeProtocolRequest.cs`, inspected at upstream
commit `9744e1f051866215611b8440c638042aa2aa2f72`.
