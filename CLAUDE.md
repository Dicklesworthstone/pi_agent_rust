# Pi Agent - Claude Code Guidelines

## Ground Rules

**CRITICAL: These rules must ALWAYS be followed.**

1. **NEVER push directly to `master`** - All changes must go through a Pull Request
2. **Always create a feature branch first** - Use `git checkout -b feature/your-change` or `fix/your-fix`
3. **Run quality checks before committing** - `cargo fmt && cargo clippy -- -D warnings && cargo test`
4. **Create a PR for review** - Use `gh pr create` to submit changes
5. **Wait for CI and review** - PRs must pass CI and be reviewed before merging
6. No emojis in codebase
7. Refrain from purple hues in frontend
8. Always test code before deployment
9. **500-Line Rule**: Any Rust source file exceeding ~500 lines of production code must be split into a directory module (`mod.rs` + sub-modules) with `pub use` re-exports to preserve existing import paths. This keeps files agent-navigable, reduces merge conflicts in parallel worktrees, and enables clean `#[cfg(feature)]` gating. See `specs/implementation/m011-agent-friendly-project-structure.md` for rationale.
10. Never commit console.logs
11. **NEVER** add references to `Claude` or `Generated with Claude Code` or similar to the code base or the pull requests.
12. **Literate Programming Principle**: All code must be self-documenting using Rust Doc comments (`///` and `//!`). Every module, struct, enum, trait, and public function must have doc comments that:
    - Explain the purpose and responsibility (the "why")
    - Provide usage examples where applicable
    - Document error conditions and edge cases
    - **No feature IDs in doc comments** (see rule 19)
13. **Feature Spec to Code Traceability**: When implementing a feature spec from `specs/features/`, add a `// Feature fNNN` code comment (not `///` or `//!`) near the item. The code should read like documentation of the feature, but feature IDs must never appear in doc comments (see rule 19).
14. Never ever start implementing a feature without a specs/feature spec unless you ask the user if you really should to this.

15. **Beads (bd)**: Use `bd` for issue tracking. All feature specs in `specs/features/` are tracked as beads with `--type feature`. When creating a new feature spec, also create a bead: `bd create "NNN: Title" -t feature -d "specs/features/NNN-name.md"`, **and** add the feature to `specs/features/f0000-feature-status.md` in the appropriate epic table (with correct progress line and summary totals). Close beads when implemented: `bd close <id>`. Check open work with `bd list` or `bd ready`.
16. **Feature Status Updates Before PR**: Prior to creating a pull request, you **must** update:
    - **a) The feature spec** (`specs/features/NNN-*.md`): Set `**Status:**` to `Done` and check off acceptance criteria for any feature completed by the PR.
    - **b) The status tracker** (`specs/features/f0000-feature-status.md`): Update the feature's status in its epic table, the epic's progress line, and the summary totals at the bottom of the file.
    - This ensures the spec files and status tracker always reflect the true state of the codebase at the time code is merged.
17. **Implementation Specs (`specs/implementation/`)**: This folder contains system specifications, fact sheets, and non-functional requirements (e.g., port allocation, thread safety, extension points, license key management). These documents are the **source of truth** for cross-cutting concerns. When making changes that affect these specs, update the relevant document to stay in sync with the codebase. When adding a new cross-cutting concern or system-wide convention, create a new `mNNN-*.md` file here., spec paths (`specs/features/...`), and internal tracker references must **never** appear in:
    - **Rust doc comments** (`///` or `//!`) -- these render in `cargo doc` output. Use plain `// Feature fNNN` code comments instead for traceability.
    - The `specs/` directory, `CLAUDE.md`, and `#[cfg(test)]` blocks are exempt (they are developer-only).

### Workflow for Every Change

```bash
# 1. Create a feature branch (NEVER work directly on master)
git checkout -b feature/my-change

# 2. Make your changes and run quality checks
cargo fmt && cargo clippy -- -D warnings && cargo test

# 3. Commit changes
git add . && git commit -m "Description of change"

# 4. Push to feature branch
git push -u origin feature/my-change

# 5. Create PR (NEVER push to master directly)
gh pr create --title "My change" --body "Description"
```

### Session Completion (Landing the Plane)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - `cargo fmt && cargo clippy -- -D warnings && cargo test`
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:

   ```bash
   git pull --rebase
   bd sync
   git push
   git status  # MUST show "up to date with origin"
   ```

5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**

- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds

## Project Overview

TODO

### Current Features

See `specs/`

## Code Quality Requirements

### Before Every Change

All code changes **must** pass the following checks:

```bash
# 1. Format code
cargo fmt

# 2. Run clippy with strict settings (must pass with no warnings)
cargo clippy -- -D warnings

# 3. Run the full test suite
cargo test

# 4. Run benchmarks to catch performance regressions
cargo bench --bench cache --bench content --bench e2e --bench markdown --bench media --bench template
```

### Clippy Configuration

The project enforces strict clippy lints. See `Cargo.toml` for the full configuration. Key requirements:

- No warnings allowed (`-D warnings`)
- Pedantic lints enabled where practical
- Security-related lints enforced

### Testing Requirements

- **Unit tests**: Every module must have inline unit tests (`#[cfg(test)]`)
- **Integration tests**: Located in `tests/` directory
- **Coverage target**: Aim for >80% code coverage
- **Property-based tests**: Use `proptest` for complex logic where applicable

### Benchmarking Requirements

- Benchmarks live in `benches/` directory using `criterion`
- **benchmark suites** must all pass before every PR:
  - To-do
- CI runs benchmarks automatically via `.github/workflows/benchmark.yml` and alerts on >30% regressions

## Project Structure

TODO

### `userdocs/` vs `docs/` Distinction

- **`userdocs/`** is the **user-facing documentation site**. It contains the docs, guides, and reference content that ships with Pi Agent. All documentation work (feature guides, prompting docs, getting started, etc.) happens here. Uses flat `content/` layout (no `main/` subdirectory).

- **`docs/`** is the **TODO**. To-do

## Module Organization

### Core Principles

1. **Separation of concerns**: Each module has a single responsibility
2. **Public API in lib.rs**: Export only what's needed for library users
3. **Error handling**: Use `thiserror` for custom error types, propagate with `?`
4. **Async-first**: Use `async`/`await` throughout for I/O operations

### Module Dependencies

TODO

## Development Workflow

### Adding a New Feature

1. Create or update the feature spec in `specs/features/`
2. Write failing tests first (TDD approach encouraged)
3. Implement the feature
4. Ensure all quality checks pass:

   ```bash
   cargo fmt && cargo clippy -- -D warnings && cargo test && cargo bench --bench cache --bench content --bench e2e --bench markdown --bench media --bench template
   ```

5. Update documentation if public API changes

### Fixing a Bug

1. Write a test that reproduces the bug
2. Fix the bug
3. Verify the test passes
4. Run full quality checks

### Performance Work

1. Add or update benchmarks in `benches/`
2. Establish baseline: `cargo bench --bench cache --bench content --bench e2e --bench markdown --bench media --bench template -- --save-baseline before`
3. Make changes
4. Compare: `cargo bench --bench cache --bench content --bench e2e --bench markdown --bench media --bench template -- --baseline before`
5. Only merge if no regressions (or regressions are justified)

## Coding Conventions

### Error Handling

```rust
// Use thiserror for error types
#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("page not found: {0}")]
    NotFound(String),

    #[error("invalid frontmatter: {0}")]
    InvalidFrontmatter(#[from] serde_yaml::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// Use Result type alias
pub type Result<T> = std::result::Result<T, ContentError>;
```

### Async Code

```rust
// Prefer async functions
pub async fn load_page(path: &Path) -> Result<Page> {
    let content = tokio::fs::read_to_string(path).await?;
    // ...
}
```

### Testing

```rust
// Inline unit tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter() {
        // ...
    }

    #[tokio::test]
    async fn test_async_operation() {
        // ...
    }
}
```

## Crate Documentation Lookup

Use this when you need to look up API signatures, types, or usage examples
for any Rust crate used in this project. Prefer local docs in `target/doc-md/`
over training data or web lookups -- they match the exact versions in `Cargo.lock`.

### Looking Up Documentation

Docs are organized as one directory per crate with Markdown files per module:

```
target/doc-md/
  index.md                    # Master index of all crates
  axum/index.md               # Crate root docs
  axum/routing.md             # axum::routing module
  tokio/sync/index.md         # tokio::sync module
  serde_json/index.md         # serde_json crate root
```

To find docs for a crate, read `target/doc-md/<crate_name>/index.md`.
For a specific module, read `target/doc-md/<crate_name>/<module>.md`.
Hyphens in crate names become underscores in directory names (e.g., `tower-http` -> `tower_http`).

### Regenerating Docs

Docs should be regenerated when `Cargo.lock` is newer than `target/doc-md/index.md`,
which means dependencies were updated.

```bash
# Full regeneration (all dependencies, including private items)
cargo +nightly doc-md --include-private

# Targeted regeneration (specific crates, faster)
cargo +nightly doc-md --include-private -p <crate1> -p <crate2>

# First-time setup (if cargo-doc-md is not installed)
rustup install nightly
cargo +nightly install cargo-doc-md
```

## Dependencies Policy

- Prefer well-maintained, minimal-dependency crates
- Security-audit dependencies with `cargo audit`
- Pin major versions in `Cargo.toml`
- Document why each dependency is needed

## CI/CD Expectations

The following should pass in CI:

```yaml
- cargo fmt --check
- cargo clippy -- -D warnings
- cargo test
- cargo bench --no-run  # Compile benchmarks
- cargo audit           # Security audit
- cargo doc --no-deps   # Documentation builds
```

## GitHub Actions Workflows

This project uses automated CI/CD pipelines to maintain code quality, especially important for multi-agent development where multiple Claude instances may be working concurrently.

### CI Pipeline (`.github/workflows/ci.yml`)

**Triggers**: All pull requests and pushes to `master`

**Job sequence** — security runs first and gates all other jobs:

```
security ──┐
           ├── check (if Rust files changed)
changes  ──┘
           └── editions (if Rust files changed)
```

**Jobs**:

1. **Security Audit** — runs first, no file-change filter, every push and PR
   - `rustsec/audit-check@v2` against `.cargo/audit.toml`
   - `check` and `editions` will not start until this passes

2. **Quality Checks** (after security passes, Rust files only)
   - `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`
   - `cargo doc --no-deps`, 500-line file size check

3. **Edition Profiles** (after security passes, Rust files only)
   - `edition-core` and `edition-pro` clippy + test matrix

**Runner policy**:

| Event | `security` + `changes` | `check` + `editions` |
|-------|------------------------|----------------------|
| Pull request | `[self-hosted, linux, x64, rust]` | `[self-hosted, linux, x64, rust]` |
| Push to master | `ubuntu-latest` | `[self-hosted, linux, x64, rust]` |

All jobs on PRs run on the self-hosted LAN runner to avoid GitHub-hosted costs.

See `specs/implementation/m014-ci-release-process.md` for full details.

### Continuous Benchmarking (`.github/workflows/benchmark.yml`)

**Triggers**: Push to `master` only (Rust files changed). Does **not** run on PRs.

**What it does**:

- Runs all 6 Criterion benchmark suites with `--output-format bencher` on the self-hosted runner
- Stores results in `gh-pages` branch as baseline via `benchmark-action/github-action-benchmark@v1`
- Alerts on >30% regressions against the stored baseline

### Release Process

Releases are triggered by pushing a semver tag. The full checklist:

```bash
# 1. Update CHANGELOG.md — add [X.Y.Z] - YYYY-MM-DD section
# 2. Bump version in Cargo.toml
# 3. cargo check  (updates Cargo.lock)
git add CHANGELOG.md Cargo.toml Cargo.lock
git commit -m "chore: release vX.Y.Z"
git tag -a vX.Y.Z -m "Release vX.Y.Z"
git push && git push origin vX.Y.Z
gh release create vX.Y.Z --title "vX.Y.Z" --notes "..."
```

The `release.yml` workflow builds 18 binaries (3 editions × 6 targets) and
uploads them plus a `checksums-vX.Y.Z.txt` to the GitHub Release automatically.

See `specs/implementation/m014-ci-release-process.md` for the full release spec.

### Claude PR Review (`.github/workflows/pr-review.yml`)

**Triggers**: When PRs are opened, updated, or reopened

**What it does**:

- Automatically reviews pull requests using Claude Sonnet 4.5
- Reads this CLAUDE.md file to understand project guidelines
- Analyzes the PR diff for:
  - Summary of changes
  - Code quality assessment
  - Potential bugs, performance issues, or security concerns
  - Suggestions for improvement
  - Recommendation (approve/request changes/reject)
- Posts detailed review as a PR comment
- Handles large PRs (>100KB) gracefully with a warning

**Setup Required**:
The Claude PR review workflow requires an Anthropic API key configured as a GitHub secret:

1. Go to repository **Settings → Secrets and variables → Actions**
2. Add a new secret:
   - Name: `ANTHROPIC_API_KEY`
   - Value: Your Anthropic API key from <https://console.anthropic.com/>

**For Multi-Agent Development**:

- Each PR gets automatically reviewed by Claude, ensuring consistency across agents
- CI must pass before merging - all agents' code must meet the same quality standards
- The automated review catches issues early, reducing back-and-forth
- PR reviews provide learning feedback for future Claude instances

### Working with the Pipelines

**Before creating a PR**:

- Run the full local CI mirror to catch all edition-specific failures before pushing:

  ```bash
  bash .github/scripts/local-ci.sh
  ```

  This runs fmt, clippy, and tests for all three feature profiles (`default`, `edition-core`, `edition-pro`) exactly as CI does.
- **CRITICAL**: `cargo clippy -- -D warnings` alone is not sufficient. It only runs with the default (all-features) profile. Unused imports inside `#[cfg(feature = "...")]` blocks only show up when that feature is disabled. Always run all three profiles.
- **Docs/specs-only PRs**: CI and benchmarks auto-skip when only `.md` or spec files change (path filtering via `dorny/paths-filter`). No need to run `cargo` checks for markdown-only changes

**When CI fails**:

- Click on the failed job in GitHub Actions to see detailed logs
- Fix the issues locally and push again
- CI will automatically re-run on new commits

**Reviewing Claude's feedback**:

- The automated Claude review is advisory - use your judgment
- It's based on the guidelines in this file, so keeping CLAUDE.md updated improves reviews
- Claude may miss context that you have - that's okay

**Updating workflows**:

- Workflow files are in `.github/workflows/`
- Test workflow changes in a feature branch first
- Changes to workflows also trigger CI validation

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
