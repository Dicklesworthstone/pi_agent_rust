# m014: CI and Release Process

**Type:** Implementation Spec
**Status:** Current
**Last Updated:** 2026-03-16

## Overview

This document describes the full CI/CD pipeline, runner allocation policy, and
release process for Accent CMS. It is the source of truth for how code moves
from a feature branch to a versioned release.

---

## Workflows

Four GitHub Actions workflows live in `.github/workflows/`:

| File | Trigger | Purpose |
|------|---------|---------|
| `ci.yml` | PR → master, push → master | Quality gate: security, lint, test |
| `benchmark.yml` | push → master (Rust files only) | Performance regression tracking |
| `release.yml` | push of `v*` tag | Cross-platform binary builds |
| `pr-review.yml` | `workflow_dispatch` only (disabled) | API-based Claude PR review |

---

## CI Pipeline (`ci.yml`)

### Job Sequence

```
security ──┐
           ├── check (if Rust files changed)
changes  ──┤
           └── editions (if Rust files changed)
```

Security runs first — it has no `needs` dependency and starts immediately.
`check` and `editions` both `needs: [security, changes]` and will not start
until the audit passes. This ensures a known-vulnerable dependency is caught
before wasting runner-minutes on compilation.

### Jobs

#### `security` — Security Audit

- **Runner:** self-hosted on PRs; `ubuntu-latest` on master pushes
- **Always runs:** no file-change filter; every push and PR is audited
- **Tool:** `rustsec/audit-check@v2` (pre-built, no `cargo install` cost)
- **Config:** `.cargo/audit.toml` — ignored advisories listed there with
  explanatory comments and tracking references

#### `changes` — Detect Changes

- **Runner:** self-hosted on PRs; `ubuntu-latest` on master pushes
- **Tool:** `dorny/paths-filter@v3`
- **Output:** `rust` flag (true when `**/*.rs`, `Cargo.toml`, or `Cargo.lock` changed)
- Markdown-only and spec-only pushes skip `check` and `editions` entirely

#### `check` — Quality Checks (Linux)

- **Runner:** `[self-hosted, linux, x64, rust]` always
- **Runs only when:** `changes.outputs.rust == 'true'`
- **Timeout:** 30 minutes
- Steps: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`,
  `cargo doc --no-deps`, `.github/scripts/check_file_sizes.sh` (500-line rule)

#### `editions` — Edition Profiles

- **Runner:** `[self-hosted, linux, x64, rust]` always
- **Runs only when:** `changes.outputs.rust == 'true'`
- **Timeout:** 20 minutes
- **Matrix:** `edition-core` (no default features) and `edition-pro`
- Steps: `cargo clippy` and `cargo test` for each edition profile
- Catches unused-import warnings that only surface with features disabled

### Runner Policy

| Event | `security` + `changes` | `check` + `editions` |
|-------|------------------------|----------------------|
| Pull request | self-hosted | self-hosted |
| Push to master | `ubuntu-latest` | self-hosted |

All jobs on PRs use the self-hosted runner to avoid GitHub-hosted runner costs
and to keep PR feedback latency low (pre-warmed Rust toolchain and sccache).

### Local CI Mirror

Before pushing a PR, run:

```bash
bash .github/scripts/local-ci.sh
```

This mirrors all three edition profiles (`default`, `edition-core`,
`edition-pro`) exactly as CI does. `cargo clippy -- -D warnings` alone is
insufficient: unused imports inside `#[cfg(feature)]` blocks only surface
when that feature is disabled.

---

## Continuous Benchmarking (`benchmark.yml`)

- **Trigger:** push to `master` when Rust files change (`.rs`, `Cargo.toml`,
  `Cargo.lock`). Does **not** run on pull requests.
- **Runner:** `[self-hosted, linux, x64, rust]` — consistent hardware
  eliminates noisy-neighbour variance that would produce false regression alerts
- **Benchmark suites:** all 6 Criterion suites (`cache`, `content`, `e2e`,
  `markdown`, `media`, `template`) with `--output-format bencher`
- **Baseline storage:** results stored in `gh-pages` branch via
  `benchmark-action/github-action-benchmark@v1`
- **Regression threshold:** >30% slowdown alerts and would fail a PR run if
  the trigger were re-enabled on PRs

---

## Release Workflow (`release.yml`)

### Trigger

Push a semver tag matching `v*`:

```bash
git tag -a v0.13.0 -m "Release v0.13.0"
git push origin v0.13.0
```

A GitHub Release must be created separately (via `gh release create` or the
GitHub UI). The workflow uploads binaries and checksums to an existing release.

### Build Matrix

18 jobs run in parallel (3 editions × 6 targets):

| Edition | Features |
|---------|---------|
| `core` | `--no-default-features --features edition-core` |
| `standard` | `--no-default-features --features edition-standard` |
| `pro` | `--no-default-features --features edition-pro` |

| Target | Runner | Notes |
|--------|--------|-------|
| `x86_64-unknown-linux-gnu` | `ubuntu-latest` | |
| `aarch64-unknown-linux-gnu` | `ubuntu-latest` | cross-compiled via `cross` |
| `x86_64-apple-darwin` | `macos-latest` | |
| `aarch64-apple-darwin` | `macos-latest` | |
| `x86_64-pc-windows-msvc` | `windows-latest` | |
| `aarch64-pc-windows-msvc` | `windows-latest` | |

### Output

- Archives: `accentcms-{edition}-{version}-{target}.tar.gz` (Linux/macOS) or `.zip` (Windows)
- Checksums: `checksums-{version}.txt` (SHA-256 for all archives)
- Both uploaded to the GitHub Release via `softprops/action-gh-release@v2`

### Release Checklist

1. **Update `CHANGELOG.md`** — move `[Unreleased]` entries under a new `[X.Y.Z] - YYYY-MM-DD` heading
2. **Bump version in `Cargo.toml`** — `version = "X.Y.Z"`
3. **Run `cargo check`** to update `Cargo.lock`
4. **Commit:** `git commit -m "chore: release vX.Y.Z"`
5. **Tag:** `git tag -a vX.Y.Z -m "Release vX.Y.Z"`
6. **Push commit + tag:** `git push && git push origin vX.Y.Z`
7. **Create GitHub Release:** `gh release create vX.Y.Z --title "vX.Y.Z" --notes "..."`

The CI pipeline runs on the release commit. The Release Binaries workflow
triggers automatically from the tag push.

---

## Security Audit Policy

- `cargo audit` runs on every push and PR (no file-change filter)
- Configuration in `.cargo/audit.toml`
- Ignored advisories must include:
  - The RUSTSEC ID
  - The reason it cannot be fixed (e.g., blocked on upstream extism release)
  - A reference to the tracking bead or spec
  - A note on when to remove the ignore entry
- **Never** add an ignore entry without a tracking reference
- See `specs/features/f128-security-dependency-upgrades.md` for the current
  set of wasmtime CVEs blocked on extism upstream

---

## Self-Hosted Runner

The `[self-hosted, linux, x64, rust]` runner is a LAN machine set up per
`specs/implementation/m007-self-hosted-runner-setup.md` and
`specs/implementation/m009-mac-mini-vm-runners-setup.md`.

Key properties:

- Rust toolchain pre-installed (no `dtolnay/rust-toolchain` action needed)
- `sccache` configured for incremental compilation across runs
- Persistent `target/` directory avoids cold rebuilds
- Ephemeral VM isolation available via OrbStack (see m009)
