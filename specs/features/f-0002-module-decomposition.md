# f-0002: Module Decomposition

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 2
> Priority: High | Effort: 3-5 days

---

## Problem

Four files contain disproportionate amounts of code, making them difficult to
navigate, review, and compile incrementally:

| File | Lines | What it contains |
|------|-------|------------------|
| `src/extensions.rs` | 50,453 | Runtime, policy engine, sandbox, traits, tests |
| `src/extensions_js.rs` | 25,360 | QuickJS bridge, hostcall dispatch, JS runtime |
| `src/session.rs` | 11,386 | JSONL format, branching, forking, HTML export, tests |
| `src/tools.rs` | 9,205 | All 8 tool implementations + shared helpers |

Total: ~96k lines in 4 files — 17% of the entire codebase.

## Design

### Phase 1: Split `src/tools.rs` into `src/tools/` module

Mirror the TypeScript structure (`core/tools/*.ts`):

```
src/tools/
  mod.rs          — ToolSet, Tool trait, shared helpers, image resize
  bash.rs         — BashTool
  read.rs         — ReadTool
  write.rs        — WriteTool
  edit.rs         — EditTool
  hashline_edit.rs — HashlineEditTool
  grep.rs         — GrepTool
  find.rs         — FindTool
  ls.rs           — LsTool
  tests.rs        — #[cfg(test)] module
```

**Approach:**
- `mod.rs` re-exports all tools so that `use crate::tools::*` still works
- Move each `struct XTool` + its `impl Tool for XTool` block into its own file
- Shared utilities (image resize, output truncation) stay in `mod.rs`
- Tests move to `tests.rs` with `#[cfg(test)]`

### Phase 2: Split `src/session.rs` into `src/session/` module

```
src/session/
  mod.rs          — Session, SessionHandle, core types
  branch.rs       — ForkPlan, branch navigation, branch summaries
  entries.rs      — SessionEntry enum, serialization
  export.rs       — to_html(), ExportSnapshot
  jsonl.rs        — JSONL read/write, atomic save, fsync
  tests.rs        — #[cfg(test)] module
```

### Phase 3: Split `src/extensions.rs` into `src/ext/` module

```
src/ext/
  mod.rs          — public API, ExtensionManager
  runtime.rs      — extension lifecycle, loading, init
  policy.rs       — permission policy engine, capability checks
  sandbox.rs      — WASM sandbox wrapper, resource limits
  traits.rs       — Extension trait, ExtensionSession trait
  events.rs       — extension event dispatch (merge with extension_events.rs)
  tests.rs        — #[cfg(test)] module
```

The existing `src/extension_*.rs` files (10 of them) remain as-is — they're
already well-scoped. Only the 50k-line monolith gets split.

### Phase 4: Split `src/extensions_js.rs`

```
src/ext_js/
  mod.rs          — JsExtension, JsRuntime wrapper
  hostcalls.rs    — hostcall dispatch table
  builtins.rs     — built-in JS APIs (fs, http, env)
  bridge.rs       — Rust↔QuickJS value conversion
  tests.rs        — #[cfg(test)] module
```

## Implementation Rules

- **No behavior changes** — this is a pure mechanical refactor
- **No API changes** — all public items keep the same `crate::` paths via re-exports
- Commit each phase separately for easy review/revert
- Run `cargo test` after each phase to verify zero regressions
- Run `cargo clippy` to catch any dead-code warnings from the split

## Acceptance Criteria

- No file exceeds 15,000 lines
- `cargo test` passes with same count as before (currently 6,187)
- `cargo clippy -- -D warnings` passes
- All public API paths unchanged (re-exported from module roots)
