# Development

## Building

Pi release builds use the exact `nightly-2026-08-31` toolchain pinned in
`rust-toolchain.toml`. The locked dependency graph requires Rust 1.95 or newer;
use the repository pin so compiler and Clippy results remain reproducible.

The one authoritative quality entry point is the registered DSR recipe; it
owns formatting, compiler checks, Clippy, tests, the installer regression,
module reachability, README freshness, and DSR contract sync, and it routes
Cargo through `rch` so a shared host never compiles locally (`AGENTS.md`
"Build, Quality, and Release Authority: DSR Only"):

```bash
# Authoritative gate (registry entry in ~/.config/dsr/repos.yaml, or the
# checked-in copy in .dsr/repos.yaml on a host that has not registered it)
dsr quality --tool pi_agent_rust
DSR_REPOS_FILE=.dsr/repos.yaml dsr quality --tool pi_agent_rust

# Cross-platform release artifacts (release operator's machine only)
dsr build pi_agent_rust
```

Do not substitute direct `cargo` or `rch exec` invocations for `dsr quality`
when validating changes, closing a Bead, or making a quality claim; add any
needed focused lane to `.dsr/repos.yaml` instead.

## Sibling Crates (Published vs Local Dev)

By default, `pi_agent_rust` depends on **published crates.io versions** of the sibling libraries:
- `asupersync`
- `rich_rust`
- `charmed-*` (bubbletea/lipgloss/bubbles/glamour)
- `fsqlite` (FrankenSQLite, pure-Rust SQLite engine)

If you want to hack on those repos locally (in lockstep), use a local-only Cargo patch. Assuming the sibling repos are checked out next to `pi_agent_rust` (e.g. `../asupersync`, `../rich_rust`, etc), add this to **your local checkout** (do not commit):

```toml
[patch.crates-io]
asupersync = { path = "../asupersync" }
rich_rust = { path = "../rich_rust" }
charmed-bubbletea = { path = "../charmed_rust/crates/bubbletea" }
charmed-lipgloss = { path = "../charmed_rust/crates/lipgloss" }
charmed-bubbles = { path = "../charmed_rust/crates/bubbles" }
charmed-glamour = { path = "../charmed_rust/crates/glamour" }
fsqlite = { path = "../frankensqlite/crates/fsqlite" }
```

## Testing

We enforce a strict "no mocks" policy for core logic. Tests use real filesystem operations (in temp dirs) and VCR-style recording for HTTP interactions.

### Unit & Integration Tests

```bash
# Authoritative: the DSR recipe runs fmt, check, clippy (default + wasm-host),
# wasm-host tests, full --all-targets tests, installer regression, reachability,
# fixture patience, README freshness, and DSR contract sync through rch.
dsr quality --tool pi_agent_rust
```

If a focused test lane is required repeatedly, register it in `.dsr/repos.yaml`
and sync `~/.config/dsr/repos.yaml` rather than running ad hoc local Cargo or
direct RCH commands. For headroom inspection before launching DSR on a busy
host, check `uptime` (1m load `< 10`) or use the read-only admission probe:

```bash
# Probe host resource posture and DSR sync status
pi doctor --only swarm --format json
```

The `pi.doctor.swarm_resource_preflight.v1` finding reports cgroup CPU quota,
cpuset size, NUMA nodes, cgroup memory limits, and scratch headroom for
`CARGO_TARGET_DIR` and `TMPDIR`. Treat any `status = fail` or non-empty
`critical_failures` list as a hard stop until both directories point under
`/data/tmp/pi_agent_rust_cargo/<agent>/` with enough free space. When the check
passes, use `recommended_budgets` as the operator ceiling for agent fanout, tool
concurrency, extension hostcall lanes, RCH verification fanout, queue depth, and
RSS budget.

Before an RCH-backed gate consumes checked-in test artifacts or emits report
bundles, run the artifact sync preflight:

```bash
python3 scripts/check_rch_artifact_sync.py --json
```

The preflight is a dry run over `.rchignore`. It fails when required artifact
paths such as `tests/ext_conformance/artifacts/` would be excluded from the
worker mirror, and the JSON output reports each required path, matched rule, and
the exact ignore line that caused a failure. Root artifact excludes must stay
anchored as `/artifacts/` and `/artifacts/**` so they do not hide nested
test-owned artifact directories.

For RCH gates that generate checked-in evidence, also bracket the remote command
with a generated-artifact postcondition:

```bash
before_manifest="/data/tmp/pi_agent_rust_cargo/${USER:-agent}/must-pass-before.json"
python3 scripts/check_rch_artifact_sync.py --mode postcondition \
  --generated-artifact tests/ext_conformance/reports/gate/must_pass_gate_verdict.json \
  --write-before-manifest "$before_manifest" --json
rch exec -- cargo test --test ext_conformance_generated --features ext-conformance -- conformance_must_pass_gate --nocapture --exact
python3 scripts/check_rch_artifact_sync.py --mode postcondition \
  --generated-artifact tests/ext_conformance/reports/gate/must_pass_gate_verdict.json \
  --before-manifest "$before_manifest" --json
```

The postcondition compares pre/post mtimes and checksums. It fails closed when a
remote generator completed but the local evidence file did not change, naming
the stale artifact and recommending a local rerun or RCH retrieval/writeback
fix.

### Conformance Tests

Conformance tests validate that Pi behaves identically to the legacy TypeScript implementation for tools, extensions, and core logic. Tests are organized in tiers:

#### Quick: Policy + Tool Conformance (no external deps)

```bash
# Tool conformance fixtures
cargo test conformance

# Extension policy negative tests (51 tests: deny/allow across modes)
cargo test --test extensions_policy_negative

# Fixture schema validation
cargo test --test ext_conformance_fixture_schema

# Artifact checksum validation
cargo test --test ext_conformance_artifacts
```

#### Full: Differential TS-Rust Oracle (requires Bun + pi-mono)

These tests run the same unmodified extension in both the legacy TypeScript runtime and the Rust QuickJS runtime, then compare registration snapshots.

**Prerequisites:**
- Bun 1.3.8 at `/home/ubuntu/.bun/bin/bun` (or on PATH)
- pi-mono npm deps installed: `cd legacy_pi_mono_code/pi-mono && npm ci`

```bash
# Official extensions (60) - differential conformance
cargo test --test ext_conformance_diff --features ext-conformance -- --nocapture

# Limit to first N official extensions (faster iteration)
PI_OFFICIAL_MAX=5 cargo test --test ext_conformance_diff --features ext-conformance -- --nocapture

# Scenario execution (tool calls, commands, events)
cargo test --test ext_conformance_scenarios --features ext-conformance -- --nocapture

# Auto-generated per-extension tests
cargo test --test ext_conformance_generated --features ext-conformance -- --nocapture

# Community + npm + third-party (weekly in CI, use --ignored)
cargo test --test ext_conformance_diff --features ext-conformance -- --ignored --nocapture

# Npm-registry differential lane (ignored opt-in, bounded to 5 by default)
rch exec -- env PI_NPM_FILTER=aliou-pi-extension-dev PI_NPM_MAX=1 \
  cargo test --test ext_conformance_diff --features ext-conformance diff_npm_manifest -- \
  --include-ignored --nocapture
```

**Environment variables:**

| Variable | Default | Purpose |
|----------|---------|---------|
| `PI_OFFICIAL_MAX` | (all) | Limit official extensions tested |
| `PI_NPM_FILTER` | (none) | Filter npm-registry extensions by `dir/entry` substring |
| `PI_NPM_MAX` | 5 | Limit the ignored npm-registry differential lane to a deterministic bounded sample |
| `PI_TS_ORACLE_TIMEOUT_SECS` | 30 | TS oracle process timeout |
| `PI_DETERMINISTIC_TIME_MS` | 1700000000000 | Fixed wall-clock for determinism |
| `PI_DETERMINISTIC_RANDOM_SEED` | 1337 | Fixed random seed |

**Reports:** Test results are written to `tests/ext_conformance/reports/` in JSONL and JSON formats.

#### Generating the Conformance Report

After running conformance tests, generate a combined per-extension report:

```bash
cargo test --test conformance_report generate_conformance_report -- --nocapture
```

This produces three output files in `tests/ext_conformance/reports/`:
- `CONFORMANCE_REPORT.md` - human-readable per-tier tables with pass/fail/N/A status
- `conformance_summary.json` - machine-readable summary with per-tier breakdowns
- `conformance_events.jsonl` - one line per extension with full metrics

#### CI Integration

| Trigger | Suite | Command |
|---------|-------|---------|
| Every PR | Fast (5 official + negative + generated) | `conformance.yml` / `conformance-fast` |
| Nightly | Full official + scenarios + schema + artifacts | `conformance.yml` / `conformance-full` + `conformance-full-scenario` |
| Weekly | Community + npm + third-party | `conformance.yml` / `conformance-weekly` |
| Every push | All non-feature-gated tests | `ci.yml` / `cargo test --all-targets` |

CI uploads conformance logs and reports as downloadable artifacts.

### Performance Report Smoke Tests

Perf/report generators should not rewrite checked-in artifacts during ordinary
test runs. Their smoke-test mode writes under `TMPDIR` by default, while
intentional evidence refreshes must run through the DSR perf/evidence lane with
an explicit `PERF_EVIDENCE_DIR=tests/perf/reports` output root (`docs/perf-budgets-recipe.md`).

### VCR Mode

Provider tests use recorded "cassettes" to avoid network calls and ensure determinism.

- **Playback (Default)**: Replays recorded responses. Fails if cassette missing (`VCR_MODE=playback` is enforced during `dsr quality --tool pi_agent_rust`).
- **Record**: Operator-only cassette refresh against real API keys (`VCR_MODE=record`).

## Quality Gates

Before committing or closing any Bead, run the single authoritative DSR quality gate (`AGENTS.md`):

```bash
dsr quality --tool pi_agent_rust
```

All formatting (`cargo fmt --check`), compiler checks, Clippy (`-D warnings` on default and `wasm-host`), full test targets, installer regression, module reachability, README evidence freshness, and DSR contract sync checks run inside that fail-closed recipe. GitHub Actions workflows (`ci.yml`, `conformance.yml`) are permanently disabled and non-authoritative.

## Project Structure

- `src/`: Core Rust source
- `tests/`: Integration and conformance tests
- `docs/`: User and developer documentation
- `legacy_pi_mono_code/`: Reference code from the original TypeScript implementation
