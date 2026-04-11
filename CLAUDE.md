# Pi Agent Rust — Claude Code Guidelines

## Ground Rules

**CRITICAL: These rules must ALWAYS be followed.**

1. **NEVER push directly to `main`** — All changes must go through a Pull Request
2. **Always create a feature branch first** — Use `git checkout -b feature/your-change` or `fix/your-fix`
3. **Run quality checks before committing** — `cargo fmt && cargo clippy -- -D warnings && cargo test`
4. **Create a PR for review** — Use `gh pr create` to submit changes
5. **Wait for CI and review** — PRs must pass CI and be reviewed before merging
6. No emojis in codebase
7. Always test code before deployment
8. **500-Line Rule**: Any Rust source file exceeding ~500 lines of production code must be split into a directory module (`mod.rs` + sub-modules) with `pub use` re-exports to preserve existing import paths. This keeps files agent-navigable, reduces merge conflicts in parallel worktrees, and enables clean `#[cfg(feature)]` gating. See `specs/implementation/m0001-agent-friendly-project-structure.md` for rationale.
9. Never commit console.logs or debug prints
10. **NEVER** add references to `Claude` or `Generated with Claude Code` or similar to the code base or the pull requests.
11. **Literate Programming Principle**: All code must be self-documenting using Rust Doc comments (`///` and `//!`). Every module, struct, enum, trait, and public function must have doc comments that:
    - Explain the purpose and responsibility (the "why")
    - Provide usage examples where applicable
    - Document error conditions and edge cases
    - **No feature IDs in doc comments** (see rule 16)
12. **Feature Spec to Code Traceability**: When implementing a feature spec from `specs/features/`, add a `// Feature fNNN` code comment (not `///` or `//!`) near the item. The code should read like documentation of the feature, but feature IDs must never appear in doc comments.
13. Never ever start implementing a feature without a specs/feature spec unless you ask the user if you really should do this.
14. **Beads (bd)**: Use `bd` for issue tracking. All feature specs in `specs/features/` are tracked as beads with `--type feature`. When creating a new feature spec, also create a bead: `bd create "NNN: Title" -t feature -d "specs/features/NNN-name.md"`. Close beads when implemented: `bd close <id>`. Check open work with `bd list` or `bd ready`.
15. **Feature Status Updates Before PR**: Prior to creating a pull request, you **must** update:
    - **a) The feature spec** (`specs/features/NNN-*.md`): Set `**Status:**` to `Done` and check off acceptance criteria for any feature completed by the PR.
    - **b) The status tracker** (`specs/features/f0000-feature-status.md`): Update the feature's status in its epic table, the epic's progress line, and the summary totals at the bottom of the file (if this file exists).
    - This ensures the spec files and status tracker always reflect the true state of the codebase at the time code is merged.
16. Spec paths (`specs/features/...`) and internal tracker references must **never** appear in:
    - **Rust doc comments** (`///` or `//!`) — these render in `cargo doc` output. Use plain `// Feature fNNN` code comments instead for traceability.
    - The `specs/` directory, `CLAUDE.md`, and `#[cfg(test)]` blocks are exempt (they are developer-only).

### Workflow for Every Change

```bash
# 1. Create a feature branch (NEVER work directly on main)
git checkout -b feature/my-change

# 2. Make your changes and run quality checks
cargo fmt && cargo clippy -- -D warnings && cargo test

# 3. Commit changes
git add <files> && git commit -m "Description of change"

# 4. Push to feature branch
git push -u origin feature/my-change

# 5. Create PR (NEVER push to main directly)
gh pr create --title "My change" --body "Description"
```

### Session Completion (Landing the Plane)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** — Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) — `cargo fmt && cargo clippy -- -D warnings && cargo test`
3. **Update issue status** — Close finished work, update in-progress items
4. **PUSH TO REMOTE** — This is MANDATORY:

   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```

5. **Clean up** — Clear stashes, prune remote branches
6. **Verify** — All changes committed AND pushed
7. **Hand off** — Provide context for next session

**CRITICAL RULES:**

- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing — that leaves work stranded locally
- NEVER say "ready to push when you are" — YOU must push
- If push fails, resolve and retry until it succeeds

## Project Overview

Rust port of the Pi Agent (AI coding agent CLI), originally written in TypeScript as part of the [pi-mono](https://github.com/badlogic/pi-mono) monorepo. The original TypeScript source lives at `packages/coding-agent` within that repo.

TODO: Add high-level architecture description, key design decisions, and target use cases.

## Build & Test

```bash
cargo check          # type-check without building
cargo build          # debug build
cargo build --release # release build
cargo test           # run all tests (~6,200 tests, ~80s)
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
- `specs/` — specifications (see Spec System below)
- `legacy_pi_mono_code/pi-mono/` — git submodule pointing to `badlogic/pi-mono` (the original TypeScript codebase for reference during porting)
- `AGENTS.md` — agent guidelines (file deletion rules, git safety, toolchain)
- `.pre-commit-config.yaml` — pre-commit hooks (fmt, clippy, tests, secrets)

### Spec System

All planning and tracking lives in `specs/` with four categories, each with its own numbering prefix and status tracker:

| Directory | Prefix | Status Tracker | Purpose |
|-----------|--------|----------------|---------|
| `specs/features/` | `fNNNN-` | `f0000-feature-status.md` | Feature specs and implementation details |
| `specs/epics/` | `eNNNN-` | `e0000-epic-status.md` | High-level investment areas and roadmaps |
| `specs/research/` | `rNNNN-` | `r0000-research-status.md` | Research documents and analysis |
| `specs/implementation/` | `mNNNN-` | `m0000-implementation-status.md` | System specs, cross-cutting concerns, non-functional requirements |

**Key implementation specs:**
- [`m0001-agent-friendly-project-structure.md`](specs/implementation/m0001-agent-friendly-project-structure.md) — rationale for the 500-line rule and module splitting strategy
- [`m0002-ci-release-process.md`](specs/implementation/m0002-ci-release-process.md) — CI pipeline, release workflow, and branch protection details

**Numbering convention:** `0000` is always the status tracker. Specs start at `0001` and increment. When creating a new spec, use the next available number in the appropriate category.

### `userdocs/` vs `docs/` Distinction

- **`userdocs/`** — the **user-facing documentation site** (TODO: create when ready). Will contain docs, guides, and reference content that ships with Pi Agent.

- **`docs/`** — internal developer documentation, extension schemas, provider snapshots, and generated artifacts. Not user-facing.

### Module Dependencies

TODO: Document the dependency graph between core modules (tools, session, providers, extensions, TUI).

## Code Quality Requirements

### Before Every Commit

Pre-commit hooks enforce these automatically:

```bash
cargo fmt --check       # formatting
cargo clippy -- -D warnings  # lint (no warnings allowed)
cargo test --lib        # unit tests (~80s)
```

Plus: trailing whitespace, YAML/TOML validation, merge conflict detection, large file blocking (>100KB), secret scanning (gitleaks).

### Testing Requirements

- **Unit tests**: Every module should have inline unit tests (`#[cfg(test)]`)
- **Integration tests**: Located in `tests/` directory (250+ files)
- **Property-based tests**: Uses `proptest` for complex logic
- **VCR recording**: `src/vcr.rs` for HTTP stream record/replay
- **Conformance testing**: Golden file validation in `src/conformance.rs`

### Benchmarking Requirements

- Benchmarks live in `benches/` directory
- TODO: Define benchmark suites and CI integration (see `specs/features/f0005-performance-profiling.md`)

## Coding Conventions

### Error Handling

```rust
// Use thiserror for error types
#[derive(Debug, thiserror::Error)]
pub enum MyError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
```

### Async Code

This project uses `asupersync` (not tokio) as the async runtime:

```rust
pub async fn load_session(path: &Path) -> Result<Session> {
    let content = asupersync::fs::read_to_string(path).await?;
    // ...
}
```

### Module Organization

1. **Separation of concerns**: Each module has a single responsibility
2. **Public API in lib.rs**: Export only what's needed for library users
3. **Error handling**: Use `thiserror` for custom error types, propagate with `?`
4. **Async-first**: Use `async`/`await` throughout for I/O operations

## CI/CD Pipeline

### CI (`.github/workflows/ci.yml`)

**Triggers**: All pull requests and pushes to `main`

Runs on 3 platforms (ubuntu, macos, windows):
- `cargo fmt --check`
- `cargo clippy -- -D warnings`
- `cargo test`

### Release (`.github/workflows/release.yml`)

**Triggers**: Tag push (`v*`)

Builds cross-platform binaries and uploads to GitHub Releases.

### Additional Workflows

- `bench.yml` — benchmark runs
- `conformance.yml` — extension conformance checks
- `fuzz.yml` — fuzzing runs

### Release Process

TODO: Document release checklist (tag, changelog, binary builds). See `specs/features/f0003-single-binary-distribution.md` and `specs/implementation/m0002-ci-release-process.md`.

### Claude PR Review

TODO: Set up `.github/workflows/pr-review.yml` for automated Claude PR reviews. Requires `ANTHROPIC_API_KEY` secret. See `specs/implementation/m0002-ci-release-process.md` for CI pipeline details.

### Branch Protection

`main` is protected:
- Requires PR (no direct pushes)
- Requires all 3 CI platform checks to pass
- Strict status checks (branch must be up to date)
- Dismisses stale reviews on new pushes

## Git Conventions

- Default branch: **main** (not master)
- All work on feature branches, merged via PR
- See `AGENTS.md` for detailed git safety rules and file deletion policy

## Dependencies Policy

- Prefer well-maintained, minimal-dependency crates
- Security-audit dependencies with `cargo audit`
- Pin major versions in `Cargo.toml`
- Document why each dependency is needed

## Development Workflow

### Adding a New Feature

1. Create or update the feature spec in `specs/features/`
2. Create a beads issue: `bd create --title="fNNNN: Title" --type=feature`
3. Write failing tests first (TDD approach encouraged)
4. Implement the feature
5. Ensure all quality checks pass
6. Create PR via `gh pr create`

### Fixing a Bug

1. Write a test that reproduces the bug
2. Fix the bug
3. Verify the test passes
4. Run full quality checks
5. Create PR

### Performance Work

1. Add or update benchmarks in `benches/`
2. Establish baseline
3. Make changes
4. Compare against baseline
5. Only merge if no regressions (or regressions are justified)
