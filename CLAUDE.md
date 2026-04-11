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
- **fd (`fd-find`)** — required at runtime for the `find` tool and related tests
- No other package manager; Cargo only

## TypeScript Reference Rule

**When a design question, naming convention, or behavioral intent is unclear in the Rust code, always consult the original TypeScript source in the submodule before guessing.**

The TypeScript codebase is the canonical reference for how features should work:

```
legacy_pi_mono_code/pi-mono/packages/coding-agent/src/
```

Key areas to cross-reference:
- `core/tools/*.ts` — tool behavior (bash, read, write, edit, grep, find, ls)
- `core/session-manager.ts` — session format, branching, persistence
- `core/extensions/` — extension loading, types, runner
- `core/model-registry.ts` — provider/model resolution and display names
- `core/compaction/` — context compaction strategy
- `modes/rpc/` — RPC protocol behavior
- `modes/interactive/` — TUI component behavior
- `core/system-prompt.ts` — system prompt construction

Use `rg` or `grep` inside the submodule to find the TypeScript equivalent of whatever you're working on in Rust. The TS code often has comments and naming that clarify the original design intent.

## Repository Structure

- `src/` — Rust source (lib + binary)
- `build.rs` — build script
- `legacy_pi_mono_code/pi-mono/` — git submodule pointing to `badlogic/pi-mono` (the original TypeScript codebase for reference during porting)
- `AGENTS.md` — agent guidelines (file deletion rules, git safety, toolchain)

## Git Conventions

- Default branch: **main** (not master)
- See `AGENTS.md` for detailed git safety rules and file deletion policy
