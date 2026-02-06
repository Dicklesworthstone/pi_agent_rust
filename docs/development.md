# Development

## Building

Pi requires Rust nightly (2024 edition).

```bash
# Build dev binary
cargo build

# Build release binary (optimized)
cargo build --release
```

## Testing

We enforce a strict "no mocks" policy for core logic. Tests use real filesystem operations (in temp dirs) and VCR-style recording for HTTP interactions.

### Unit & Integration Tests

```bash
# Run all tests
cargo test

# Run specific module
cargo test config
cargo test session
```

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
```

**Environment variables:**

| Variable | Default | Purpose |
|----------|---------|---------|
| `PI_OFFICIAL_MAX` | (all) | Limit official extensions tested |
| `PI_TS_ORACLE_TIMEOUT_SECS` | 30 | TS oracle process timeout |
| `PI_DETERMINISTIC_TIME_MS` | 1700000000000 | Fixed wall-clock for determinism |
| `PI_DETERMINISTIC_RANDOM_SEED` | 1337 | Fixed random seed |

**Reports:** Test results are written to `tests/ext_conformance/reports/` in JSONL and JSON formats.

#### CI Integration

| Trigger | Suite | Command |
|---------|-------|---------|
| Every PR | Fast (5 official + negative + generated) | `conformance.yml` / `conformance-fast` |
| Nightly | Full official + scenarios + schema + artifacts | `conformance.yml` / `conformance-full` + `conformance-full-scenario` |
| Weekly | Community + npm + third-party | `conformance.yml` / `conformance-weekly` |
| Every push | All non-feature-gated tests | `ci.yml` / `cargo test --all-targets` |

CI uploads conformance logs and reports as downloadable artifacts.

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
cargo clippy --all-targets -- -D warnings

# Tests
cargo test --all-targets
```

## Project Structure

- `src/`: Core Rust source
- `tests/`: Integration and conformance tests
- `docs/`: User and developer documentation
- `legacy_pi_mono_code/`: Reference code from the original TypeScript implementation