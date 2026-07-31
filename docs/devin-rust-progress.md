# Pi-Devin Rust parity progress

## Baseline

- Upstream: `Dicklesworthstone/pi_agent_rust`
- Pinned commit: `590d61899ae64e172f15d919632a9134ddec6fb6`
- Upstream version: `0.1.23`
- Implementation branch: `devin-rust-core`
- Writable fork: `OnlineChefGroep/pi_agent_rust`

The pristine `--all-features` check exposed an upstream `wasm-host` failure:
21 `Future + Send` errors converge on an `asupersync::sync::MutexGuard` held
across `await` in `src/extensions.rs`. This predates Pi-Devin changes. The
default-feature gate was stopped before completion when the project policy
changed to CI-only Rust validation.

No further Cargo, rustc, clippy, rustfmt, or release builds run on the local
workstation. Rust verification is performed by GitHub Actions.

## Contract evidence

`tests/fixtures/devin_cli/tool_schema_manifest.json` stays the pinned contract
source. It is historical evidence from four ATIF-v1.7 transcripts exported by
Devin `3000.2.17`, not a parity claim against the `3000.3.22` binary that was
installed when the transcripts were extracted.

That manifest records only a 12-hex-character SHA-256 prefix per tool. Hashing
the same canonical schema does detect most drift, but a prefix is not enough
for exact validation: a digest has no recoverable preimage, so the schema can
neither be reconstructed from it nor compared field by field.
`tests/fixtures/devin_cli/process_tool_parameter_schemas.json` therefore
pins the **full** JSON Schema for each of the five process tools, and
`tests/devin_contract.rs` asserts the registered `ToolRegistry` entries match
those schemas exactly.

Those full schemas are Pi Rust's own native contract for the pinned Devin tool
names. The fixture states this explicitly
(`reproduces_upstream_parameter_schemas: false`, and
`upstream_prefix_reproduced: false` per tool), records the upstream digest
alongside each schema, and is guarded by a test that fails if either claim is
ever flipped without recovered preimages.

A transcript exported by the installed `3000.3.22` binary was searched for and
**not** accepted: no candidate carried both a recordable provenance chain
(origin, exporting binary version, export time) and a digest that could be
pinned here. Recording unverifiable evidence is worse than an acknowledged gap,
so the four `3000.2.17` transcripts remain the only pinned evidence and the
gap is stated in the fixture rather than papered over.

## Process supervisor

One session-owned supervisor (`src/devin/process.rs`) backs all five pinned
Devin process tools (`src/devin/process_tools.rs`): `exec`, `shell_command`,
`get_output`, `write_to_process`, `kill_shell`. It reuses the primitives the
native `bash` tool already relies on rather than adding a second subprocess
stack: `command_with_default_sigpipe_in_dir`, process-group isolation, the
`sysinfo` group/tree termination helpers, `AgentCx` cancellation, `truncate_tail`,
and the shared temp-artifact cleanup in `crate::tools::cleanup_temp_files`.

- Registry entries carry a unique process id, command, cwd, start/end time,
  status, exit code, pid and process-group id, stdin state, and bounded
  stdout/stderr buffers with byte and drop counters.
- Foreground runs stream through the existing `ToolUpdate` route; background
  runs return a process id immediately and stay observable through
  `get_output`, which is incremental and reports bytes evicted from the ring.
- `write_to_process` writes stdin and fails with a specific reason for unknown
  ids, exited processes, and closed stdin.
- `kill_shell` SIGTERMs the whole process group, then SIGKILLs the group and
  descendant tree after a short grace period.
- Session cancellation, timeouts, and `ProcessSupervisor::shutdown` all
  terminate owned process groups. Detachment is opt-in per call and recorded on
  the registry entry, so an exemption from cleanup is always auditable.
- Output above the in-memory budget spills to a `pi-devin-proc-*.log` artifact;
  audit records reference the artifact instead of carrying the output.

## Policy integration

- The five tools register on the existing `ToolRegistry` and each calls
  `ToolPolicyEngine::evaluate` itself, because ACP and RPC drive the registry
  directly and must not bypass the gate. Re-evaluation is safe: audit records
  are keyed by `call_id` and upserted.
- Plan mode denies `exec`, `shell_command`, `write_to_process`, and
  `kill_shell` (`get_output` stays available as a read-only tool).
- Normal and Smart return `Ask`. A tool reached with an unresolved `Ask` fails
  closed rather than executing.
- Bypass runs the path checks before it auto-allows: `validate_paths` now
  resolves the process working directory (`cwd`, `working_dir`, `workingDir`)
  at write strength, so bypass skips the prompt without skipping the path
  argument check. **Bypass is not a contained mode.** A resolved `cwd` is a
  check on one argument, not containment of the spawned process: the child
  keeps every filesystem, process, and network capability the host OS grants
  the agent, and network calls name no path at all. Enforced OS-level process
  and network restrictions for Bypass are a blocking gap, tracked as gap 8
  below; until they exist, Bypass must be treated as an explicitly
  uncontained, operator-selected mode.
- Autonomous process execution requires a genuinely active sandbox.
  `SandboxStatus::Active` alone is only a claim, so `DevinSessionState` now also
  records `sandbox_backend`; without a named backend the decision is `Deny`,
  never `Ask` or `Allow`.

## Audit lifecycle

- Policy evaluation opens at most one record per `call_id`. `AuditLog::push`
  upserts by `call_id` and refuses to reopen a closed record, so a call can
  never produce two rows.
- Execution closes that same record through `mark_allowed`, `complete`, or
  `complete_if_open`, reaching `allowed`, `denied`, `succeeded`, `failed`,
  `cancelled`, or the newly added `timed_out`.
- `complete_if_open` lets the agent loop record a generic outcome without
  overwriting a more specific status the tool already reported.
- Records store salted argument hashes, redacted errors, and artifact
  references. Raw arguments, secret-bearing commands, and unbounded stdout are
  never retained.
- The hash salt is generated per `AuditLog` and never persisted. Argument
  hashes are therefore comparable only **inside one log**: they correlate
  repeated calls within a session and are useless as cross-session
  fingerprints.

## Implemented evidence

- Four ATIF-v1.7 transcripts exported by Devin `3000.2.17` expose the same 28
  function-calling tools. The installed binary at extraction time was
  `3000.3.22`; no current-version transcript was available.
- All four transcripts have identical JSON-schema hashes for every tool.
- `AgentMode` and `PermissionMode` are independent, session-scoped values.
- Devin mode, sandbox, workspace, and scope state round-trips through versioned
  custom session entries.
- Plan and Ask mode restrictions are policy decisions, not prompt decoration.
- Autonomous mode rejects activation without an active OS sandbox.
- Tool policy validates object arguments, classifies effects and risk, checks
  workspace/scoped paths, rejects traversal and symlink escapes, and returns
  allow, ask, deny, or sandbox.
- Native agent tool execution can use the same central policy gate before
  approvals, extension hooks, and tool execution.
- Audit records retain per-log salted argument hashes instead of raw arguments.

## Remaining gaps

1. Expose persisted Devin state through TUI, ACP, and RPC.
2. Register full transcript-derived schemas and migrate the existing eight
   tools behind the same canonical registry. The five process tools now pin
   full schemas; the remaining tools still pin hashes only.
3. Implement persistent plan/todo tools. Process supervision is done; a
   sandbox execution adapter for `PolicyAction::Sandbox` is not, so autonomous
   mode still fails closed at the executor.
4. Implement managed subagents, MCP, skills, hooks, and web/browser adapters.
5. Add disabled-by-default cloud XML parsing with no direct execution path.
6. Complete file mutation hashes/diffs and persistent audit/recovery sinks.
7. Run the end-to-end repository, plan, edit, background process, subagent,
   and MCP smoke test in CI.
8. **Blocking:** Bypass has no OS-level containment. Path checks constrain the
   arguments a tool is called with; they do not restrict what a spawned process
   may then read, write, execute, or reach over the network. Bypass cannot be
   described as contained until a process/network sandbox backend enforces
   those limits, which is the same adapter gap that keeps `PolicyAction::Sandbox`
   failing closed in gap 3.

## CI reproduction

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --test devin_contract --test devin_session_state
cargo test --test devin_process_supervisor
cargo test devin::
cargo test --all-targets
cargo build --release
```

The optional full-feature upstream defect remains separately reproducible with:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```
