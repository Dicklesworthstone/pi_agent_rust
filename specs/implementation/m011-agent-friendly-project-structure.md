# M011: Agent-Friendly Project Structure

**Related Research:** [R044: Project Reorganization](../research/r044-project-reorganization.md)
**Related Feature:** [F083: Compile Optimization and Module Structure](../features/f083-compile-optimization.md)
**Date:** 2026-03-12

## Context

This document re-evaluates the findings of Research R044 (monorepo
reorganization) and Feature F083 (module splitting) from the perspective of
AI agent productivity. The question: **what project structure is easiest for
agents like Claude Code to work with?**

The findings are grounded in direct operational experience working in this
codebase as Claude Code across multiple sessions, not theoretical analysis.

## Agent Constraints That Drive Structural Preferences

AI coding agents interact with codebases through a constrained set of
operations. Understanding these constraints explains why certain structures
are better than others.

| Constraint | Impact on structure |
|------------|-------------------|
| **Context window** | Every line read consumes tokens. Smaller files mean less waste when reading a module to edit 10 lines within it. |
| **Edit tool requires unique strings** | In a 3,000-line file, matching a unique `old_string` often requires including extra surrounding context. In a 300-line file, most function signatures are unique on their own. |
| **Glob/Grep for discovery** | Agents locate code via `Glob("**/media.rs")` or `Grep("serve_media")`. Predictable filenames and small files make this fast and precise. |
| **Parallel agent work** | Multiple agents in worktrees will conflict on the same file. Smaller files reduce merge conflict surface area. |
| **No IDE navigation** | Agents cannot "go to definition" -- they search by text. Clear module names serve as a substitute for IDE navigation. |
| **Single CLAUDE.md** | The project guidelines file is the agent's primary onboarding document. Fragmenting it across locations increases the chance of stale or contradictory instructions. |

## F083: Module Splitting -- The Highest-ROI Change

Feature F083 split three mega-files into directory modules with focused
sub-modules:

- `handlers.rs` (3,391 lines) -> `handlers/` (10 files, 80-800 lines each)
- `template.rs` (2,446 lines) -> `template/` (10 files, 100-400 lines each)
- `config.rs` (2,126 lines) -> `config/` (12 files, 40-400 lines each)

### Why This Is the Most Agent-Friendly Refactor

**Context efficiency (8x improvement).** Reading `handlers/media.rs`
(400 lines) to edit a media handler consumes ~8x fewer tokens than reading
the former monolithic `handlers.rs` (3,391 lines). Agents frequently read
entire files to understand context before editing. Smaller files make this
economical.

**Edit precision.** The `Edit` tool fails when `old_string` is not unique
in the file. In a 3,000-line file with multiple similar function signatures,
agents must include extra surrounding lines to disambiguate. In a 300-line
file, function signatures are almost always unique.

**Discovery by filename.** After the split, `Glob("**/handlers/media.rs")`
instantly tells an agent where media handling lives. Before the split,
the agent would need `Grep("serve_media", "src/server/handlers.rs")`
followed by reading surrounding context to understand function boundaries.

**Merge conflict reduction.** When two agents work in parallel worktrees
(per M006), small files have fewer conflicts. Two agents modifying
`handlers/media.rs` and `handlers/taxonomy.rs` respectively will never
conflict. Two agents editing different sections of one 3,391-line file
almost certainly will.

**Re-exports are essential.** F083 used `mod.rs` re-exports to preserve
all existing import paths. This is critical: a structural refactor that
requires updating 50 import sites is dangerous for agents because any
missed update causes compilation failure. Re-exports make the split
invisible to consumers.

### Guideline Derived from F083

**Any Rust file exceeding ~500 lines of production code should be split
into a directory module with sub-modules and re-exports.** This is the
single highest-ROI structural change for agent productivity. Apply it
proactively as files grow, not retroactively when they become unmanageable.

## R044: Monorepo Reorganization -- Defer Until Triggered

Research R044 proposes reorganizing from the current flat structure to a
`crates/`, `registry/`, `sites/` layout. The agent-productivity impact
is mixed.

### Changes That Help Agents

| Change | Benefit |
|--------|---------|
| Consolidating `site-*` into `sites/` | Fewer top-level entries to scan, predictable naming |
| Removing dead weight (`site/`, misplaced `static/`) | Less noise in Glob/Grep results |
| Standard `crates/` convention | Agents recognize this Rust monorepo pattern immediately |

### Changes That Hurt Agents

| Change | Cost |
|--------|------|
| **Deeper nesting** | `crates/pi_agent_rust/src/server/handlers/media/mod.rs` is 5 levels deep vs the current 4. Longer paths in every tool call, harder to type and reason about. |
| **Scattered specs** | Per-crate `docs/` and `specs/` fragments the search space. Currently `Glob("specs/**/*.md")` finds everything. After R044, an agent must search `specs/` + `crates/cachee/specs/` + `crates/proto/specs/` + potentially more. |
| **Multiple Cargo.toml** | Agents must determine which `Cargo.toml` to modify when adding a dependency. Workspace inheritance reduces but does not eliminate this. |
| **Path churn** | Every `include_dir!` macro, CI workflow path, CLAUDE.md reference, and config path must be updated atomically. High risk of missed references that cause silent failures. |
| **Build system proliferation** | Adding Justfile + mise + potential Buck2 means agents must learn which tool orchestrates which task. `cargo test` is universally understood; `just quality` requires reading the Justfile. |

### Current Structure Is Already Agent-Optimal

For a single-binary project at ~32K lines, the flat structure is ideal:

- `Glob("src/**/*.rs")` finds all code
- `Grep("serve_media")` locates any function instantly
- One `Cargo.toml` to modify
- One `specs/` tree to search
- Simple mental model: `src/` = code, `specs/` = specs, `site-docs/` = docs

R044's own decision gates confirm the reorganization is not yet needed:

| Trigger | Threshold | Current state |
|---------|-----------|---------------|
| Cachee shares types with pi_agent_rust | When it happens | Cachee does not exist |
| Compile time exceeds 60s incremental | > 60s | Well below |
| Workspace crate count | > 8 crates | 2 members |

**Recommendation:** Defer R044 until at least one trigger is met. The flat
structure is actively better for agents right now.

## Structural Principles for Agent-Friendly Codebases

These principles are derived from operational experience and apply beyond
Accent CMS to any Rust project worked on by AI agents.

### 1. Split files at ~500 lines

Files above 500 lines of production code waste agent context window tokens
and increase edit-collision risk. Split into directory modules with
`mod.rs` re-exports. The re-exports are non-negotiable -- they prevent
downstream path churn.

### 2. Keep specs centralized

A single `specs/` tree with consistent naming (`f083-*.md`, `r044-*.md`,
`m011-*.md`) is dramatically easier for agents to search than specs
scattered across `crates/*/specs/`. Agents rely on `Glob("specs/**/*.md")`
as a primary discovery mechanism.

### 3. One authoritative CLAUDE.md at root

CLAUDE.md is the agent's onboarding document. It must be comprehensive,
accurate, and singular. Do not fragment it into per-crate or per-directory
files. If the project grows to need per-crate build instructions, add a
section to the root CLAUDE.md rather than creating `crates/cachee/CLAUDE.md`.

### 4. Prefer shallow directory trees

Each additional nesting level adds cognitive overhead for agents. Prefer
3-4 levels of depth over 5-6. Flat-but-organized beats deeply-nested.

### 5. Use predictable, greppable naming

The `f`-prefixed feature numbering (`f083-compile-optimization.md`) is
excellent for agents. Predictable patterns allow agents to construct
filenames without searching first. Apply this principle to module names,
test files, and configuration.

### 6. Minimize build tool surface area

`cargo` is the one build tool every Rust-aware agent understands deeply.
Justfile as a thin wrapper for multi-step recipes is acceptable. Buck2,
Bazel, or custom build scripts introduce tools that agents have limited
training data on. Add them only when Cargo demonstrably cannot do the job.

### 7. Co-locate tests with implementation

Inline `#[cfg(test)]` modules within each source file (Rust convention)
are ideal for agents. The agent reads one file and sees both implementation
and tests. Separate test files (`tests/test_handlers.rs`) force a second
lookup and risk test-implementation drift.

### 8. Avoid workspace splits for single-product repos

Cargo workspaces add value when multiple binaries share code. For a
single binary with feature flags, the workspace overhead (multiple
`Cargo.toml`, path dependencies, feature unification edge cases) hurts
agents more than it helps. Split into workspace members only when
concrete reuse demands it.

## Checklist: When to Reorganize

Before undertaking any structural reorganization, verify that at least
one concrete trigger is met -- not anticipated, but met:

- [ ] A second binary (e.g., cachee) exists and shares types
- [ ] Incremental compile time exceeds 60 seconds
- [ ] Three or more languages produce WASM artifacts
- [ ] Multiple CI runners need a shared build cache
- [ ] Per-crate specs have accumulated and global `specs/` is unwieldy

If none are checked, keep the current structure and focus on file-level
hygiene (the F083 approach) instead.

## Summary

| Approach | Agent impact | Recommendation |
|----------|-------------|----------------|
| F083 module splitting | Strongly positive | Apply proactively to any file > 500 lines |
| R044 site consolidation | Mildly positive | Do when convenient |
| R044 crate extraction | Neutral to negative (today) | Defer until triggered |
| R044 per-crate specs | Negative | Avoid; keep specs centralized |
| R044 build tool additions | Mildly negative | Justify + limit `cargo` stays primary |

The most agent-friendly codebase is one with small focused files, shallow
directories, centralized documentation, and a single build tool. Structural
complexity should be added only when concrete triggers demand it, not in
anticipation of future needs.
