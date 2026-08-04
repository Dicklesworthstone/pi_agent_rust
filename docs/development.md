# Development

## Building

Pi release builds use the exact `nightly-2026-07-05` toolchain pinned in
`rust-toolchain.toml`. The locked dependency graph requires Rust 1.95 or newer;
use the repository pin so compiler and Clippy results remain reproducible.

```bash
# Build dev binary
rch exec -- cargo build

# Build release binary (optimized)
rch exec -- cargo build --release
```

## Sibling Crates (Published vs Local Dev)

By default, `pi_agent_rust` depends on **published crates.io versions** of the sibling libraries:
- `asupersync`
- `rich_rust`
- `charmed-*` (bubbletea/lipgloss/bubbles/glamour)
- `sqlmodel-*` (core/sqlite)

If you want to hack on those repos locally (in lockstep), use a local-only Cargo patch. Assuming the sibling repos are checked out next to `pi_agent_rust` (e.g. `../asupersync`, `../rich_rust`, etc), add this to **your local checkout** (do not commit):

```toml
[patch.crates-io]
asupersync = { path = "../asupersync" }
rich_rust = { path = "../rich_rust" }
charmed-bubbletea = { path = "../charmed_rust/crates/bubbletea" }
charmed-lipgloss = { path = "../charmed_rust/crates/lipgloss" }
charmed-bubbles = { path = "../charmed_rust/crates/bubbles" }
charmed-glamour = { path = "../charmed_rust/crates/glamour" }
sqlmodel-core = { path = "../sqlmodel_rust/crates/sqlmodel-core" }
sqlmodel-sqlite = { path = "../sqlmodel_rust/crates/sqlmodel-sqlite" }
```

## Testing

We enforce a strict "no mocks" policy for core logic. Tests use real filesystem operations (in temp dirs) and VCR-style recording for HTTP interactions.

### Unit & Integration Tests

```bash
# Run all tests
rch exec -- cargo test

# Run specific module
rch exec -- cargo test config
rch exec -- cargo test session
```

For multi-agent sessions, treat `rch exec --` as mandatory for compilation commands. Use
`./scripts/smoke.sh --require-rch` and `./scripts/ext_quality_pipeline.sh --require-rch`
to avoid accidental local compile storms. For ad hoc Cargo gates, prefer the
headroom wrapper because it emits a JSON admission decision before running:

```bash
# Probe whether a heavy gate is safe to start without running it
./scripts/cargo_headroom.sh --runner auto --admit-only clippy --all-targets -- -D warnings

# Run through rch with target/tmp directories outside the repo
PI_CARGO_AGENT_SUFFIX="$USER" ./scripts/cargo_headroom.sh --runner rch clippy --all-targets -- -D warnings
```

In `--runner auto` mode, the wrapper falls back locally only for safe local
commands such as `cargo fmt` or when the operator passes
`--allow-local-fallback` / `PI_CARGO_ALLOW_LOCAL_FALLBACK=1`. If `rch` is
missing, saturated, or unhealthy for a heavy command, the wrapper returns a
machine-readable `backoff` decision instead of silently starting a broad local
Cargo run.

Before starting a swarm or a heavyweight all-target gate, inspect the host
resource budget:

```bash
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
`cargo test` runs. Their smoke-test mode writes under `TMPDIR` by default, while
intentional evidence refreshes must pass an explicit output root:

```bash
PERF_EVIDENCE_DIR=tests/perf/reports \
  rch exec -- cargo test --test perf_comparison generate_perf_comparison -- --nocapture
```

### VCR Mode

Provider tests use recorded "cassettes" to avoid network calls and ensure determinism.

- **Playback (Default)**: Replays recorded responses. Fails if cassette missing.
- **Record**: Makes real API calls and saves cassettes.

```bash
# Run in playback mode (CI default)
VCR_MODE=playback cargo test

# Record new cassettes (requires API keys)
export ANTHROPIC_API_KEY=...
VCR_MODE=record cargo test provider_streaming
```

## Quality Gates

Before submitting a PR, ensure all gates pass:

```bash
# Format check
cargo fmt --check

# Lint check (deny warnings)
rch exec -- cargo clippy --all-targets -- -D warnings

# Tests
rch exec -- cargo test --all-targets
```

## Project Structure

- `src/`: Core Rust source
- `tests/`: Integration and conformance tests
- `docs/`: User and developer documentation
- `legacy_pi_mono_code/`: Reference code from the original TypeScript implementation
