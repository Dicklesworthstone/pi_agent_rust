# Development

This guide summarizes the build/test workflow and the quality gates expected for
changes in this repo.

## Prerequisites

- Rust nightly (2024 edition).
- `cargo`, `rustfmt`, `clippy`.
- `fd` for the `find` tool (optional but recommended).
- `rg` for faster grep (optional).

## Build

```bash
cargo build
cargo build --release
```

## Quality Gates (required after code changes)

```bash
# Formatting
cargo fmt --check

# Compiler warnings/errors
cargo check --all-targets

# Clippy (pedantic + nursery are enabled)
cargo clippy --all-targets -- -D warnings
```

## Tests

```bash
# All tests
cargo test

# With output
cargo test -- --nocapture

# Focused modules
cargo test sse::tests
cargo test tools::tests
cargo test conformance
```

## Conformance Fixtures

Conformance tests are fixture-driven and live under `tests/conformance/fixtures/`.
Each fixture defines setup steps, tool input, and expected outputs. The runner
loads fixtures and asserts stable, deterministic behavior.

Run them with:

```bash
cargo test conformance
```

## Provider Streaming (VCR)

Provider streaming tests use VCR-style recorded cassettes for determinism.
Cassettes live under `tests/fixtures/vcr/`.

```bash
# Record new cassettes (requires live API access)
VCR_MODE=record cargo test provider_streaming::anthropic

# Playback from recorded cassettes (CI-safe)
VCR_MODE=playback cargo test provider_streaming::anthropic
```

Common env vars:

- `VCR_MODE=record|playback|auto`
- `VCR_CASSETTE_DIR=tests/fixtures/vcr`

## Useful Config Paths

- Global config: `~/.pi/agent/settings.json`
- Project config: `.pi/settings.json`
- Sessions: `~/.pi/agent/sessions/` (override via `PI_SESSIONS_DIR`)
- Auth: `~/.pi/agent/auth.json`
