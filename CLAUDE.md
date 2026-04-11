# CLAUDE.md — pi_agent_rust

## Project Overview

Rust port of the Pi Agent (AI coding agent CLI), originally written in TypeScript as part of the [pi-mono](https://github.com/badlogic/pi-mono) monorepo. The original TypeScript source lives at `packages/coding-agent` within that repo.

## Build & Test

```bash
cargo check          # type-check without building
cargo build          # debug build
cargo build --release # release build
cargo test           # run all tests (~6160 tests, ~80s)
```

### Requirements

- **Rust nightly** (edition 2024, rust-version 1.85+) — see `rust-toolchain.toml`
- **ripgrep (`rg`)** — required at runtime and for ~14 grep-related tests
- No other package manager; Cargo only

### Known Failing Tests (as of 2026-04-11)

7 pre-existing test failures unrelated to recent changes:

- `built_in_models_preserve_legacy_model_display_names` — display name mismatch for `openrouter/openrouter/auto`
- `rpc_spill_file_hard_limit_abandons_partial_spill_file` — spill file cleanup assertion
- `test_bash_hard_limit_retains_partial_spill_file` — same spill file issue
- `select_model_and_thinking_preserves_restore_warning_when_defaulting_for_setup`
- `select_model_and_thinking_preserves_restore_warning_when_using_config_default`
- `suggest_slash_alone_returns_all_builtins` — autocomplete test
- `dispatcher_tool_find_discovers_files` — extension dispatcher

## Repository Structure

- `src/` — Rust source (lib + binary)
- `build.rs` — build script
- `legacy_pi_mono_code/pi-mono/` — git submodule pointing to `badlogic/pi-mono` (the original TypeScript codebase for reference during porting)
- `AGENTS.md` — agent guidelines (file deletion rules, git safety, toolchain)

## Git Conventions

- Default branch: **main** (not master)
- See `AGENTS.md` for detailed git safety rules and file deletion policy
