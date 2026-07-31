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

## Proven parity

- The live local Devin CLI exposes 28 function-calling tools in four available
  transcript fixtures.
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
- Audit records retain argument hashes instead of raw arguments.

## Remaining gaps

1. Expose persisted Devin state through TUI, ACP, and RPC.
2. Register full transcript-derived schemas and migrate the existing eight
   tools behind the same canonical registry.
3. Implement process supervision and persistent plan/todo tools.
4. Implement managed subagents, MCP, skills, hooks, and web/browser adapters.
5. Add disabled-by-default cloud XML parsing with no direct execution path.
6. Complete file mutation hashes/diffs and persistent audit/recovery sinks.
7. Run the end-to-end repository, plan, edit, background process, subagent,
   and MCP smoke test in CI.

## CI reproduction

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --test devin_contract
cargo test devin::
cargo test --all-targets
cargo build --release
```

The optional full-feature upstream defect remains separately reproducible with:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```
