# Dependency Upgrade Log

**Date:** 2026-02-02  |  **Project:** pi_agent_rust  |  **Language:** Rust

## Summary
- **Updated:** 4  |  **Skipped:** 3  |  **Failed:** 1  |  **Replaced:** 1  |  **Needs attention:** 0

## Pre-Update Status

### Path Dependencies (Skip - local development)
- `asupersync` - path dependency (fixed Clone/Send issues during this session)
- `rich_rust` - path dependency
- `bubbletea` - path dependency
- `lipgloss` - path dependency
- `bubbles` - path dependency
- `glamour` - path dependency

### Crates.io Dependencies
| Crate | Current | Target | Status |
|-------|---------|--------|--------|
| crossterm | 0.28 | 0.29 | Updated |
| sysinfo | 0.32 | 0.36 | Updated |
| criterion | 0.5 | 0.7 | Updated |
| dirs | 5 | 6 | Updated |
| reqwest | 0.12 | 0.13 | Failed |
| rusqlite | 0.33 | - | Replaced with sqlmodel-sqlite |
| time | (transitive) | | Skipped (Rust 1.87+) |
| vergen-gix | 1 | | Skipped (Rust 1.87+) |

## Updates

### crossterm: 0.28 → 0.29
- **Breaking:** Minor event handling changes
- **Tests:** Passed (when project compiles)
- **Notes:** Compatible upgrade, no code changes needed

### sysinfo: 0.32 → 0.36
- **Breaking:** Some API changes (system refresh methods)
- **Tests:** Passed (when project compiles)
- **Notes:** Project only uses basic system info, no changes needed

### criterion: 0.5 → 0.7
- **Breaking:** `black_box` deprecated in favor of `std::hint::black_box`
- **Tests:** Passed
- **Migration:** Changed `criterion::black_box` to `std::hint::black_box` in benches/tools.rs

### dirs: 5 → 6
- **Breaking:** `executable_dir` removed
- **Tests:** Passed (when project compiles)
- **Notes:** Project doesn't use `executable_dir`, safe upgrade

## Failed

### reqwest: 0.12 → 0.13
- **Reason:** `reqwest-eventsource` v0.6.0 depends on reqwest 0.12, causing version conflict
- **Error:** Type mismatch - `RequestBuilder` from different reqwest versions
- **Affected files:** anthropic.rs, azure.rs, gemini.rs, openai.rs
- **Action:** Stayed on 0.12
- **Resolution path:** Wait for reqwest-eventsource to update, or migrate SSE to asupersync (already planned per AGENTS.md)

## Replaced

### rusqlite → sqlmodel-sqlite
- **Reason:** User requested dogfooding their own sqlmodel_rust library
- **Changes:**
  - Cargo.toml: Replaced `rusqlite = { version = "0.33", features = ["bundled"] }` with `sqlmodel-sqlite` and `sqlmodel-core` path dependencies
  - session_index.rs: Rewrote to use `SqliteConnection`, `Value`, and `Row` types from sqlmodel-sqlite/sqlmodel-core
  - error.rs: Changed `#[from] rusqlite::Error` to `#[from] sqlmodel_core::Error`
- **sqlmodel-sqlite modifications:** Made `query_sync()` and `execute_sync()` public for sync usage
- **Tests:** All pass
- **Benefits:** Consistent with asupersync ecosystem, eliminates external SQLite dependency

## Skipped

### time (transitive dependency)
- **Reason:** Newer versions require Rust 1.87+

### vergen-gix: 1.x
- **Reason:** Newer versions require Rust 1.87+, current version works

### arc-swap: 1.8.0 → 1.8.1
- **Reason:** Patch update, handled by lockfile

## Path Dependency Fixes

### asupersync - Clone/Send trait fixes
- **Issue:** `Cx<Caps>` type couldn't derive `Clone` (needed `Caps: Clone`) and wasn't `Send`
- **Root cause:** `PhantomData<Caps>` requires bounds that marker types don't need
- **Fix 1:** Manual `Clone` impl without `Caps: Clone` bound
- **Fix 2:** Changed `PhantomData<Caps>` to `PhantomData<fn() -> Caps>` for auto `Send+Sync`
- **Fix 3:** Moved `current()` and `set_current()` to `impl Cx<cap::All>` block (not generic)
- **Files:** `/data/projects/asupersync/src/cx/cx.rs`

## Current Build Status

✓ Project compiles successfully (`cargo check` passes)
✓ All tests pass (`cargo test`)
✓ Clippy passes (only pre-existing warnings in test files)
✓ Formatting correct (`cargo fmt --check` passes)

---
*Log created by library-updater skill*
