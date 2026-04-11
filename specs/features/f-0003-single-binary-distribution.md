# f-0003: Single-Binary Distribution

> Epic: [Rust Investment Roadmap](../epics/rust-investment-roadmap.md) — Item 3
> Priority: High | Effort: 1-2 weeks

---

## Problem

The Rust port's primary advantage over the TypeScript original is zero runtime
dependencies — a single binary you can `curl` and run. This advantage is only
realized if distribution is polished. Currently:

- CI exists (`.github/workflows/release.yml`) but needs verification on the fork
- No `pi update` self-update command
- Shell completions are available via `clap_complete` dep but not wired up
- Binary size budget (<20MB) is defined but not measured in CI

## Features

### 3a. Verify Cross-Platform Release Pipeline

**What exists:** `.github/workflows/release.yml` builds on tag push with
matrix strategy. Verify it covers:

| Target | Runner | Linking |
|--------|--------|---------|
| `x86_64-unknown-linux-gnu` | ubuntu-latest | dynamic glibc |
| `x86_64-unknown-linux-musl` | ubuntu-latest | static musl |
| `aarch64-unknown-linux-gnu` | ubuntu-latest (cross) | dynamic glibc |
| `aarch64-apple-darwin` | macos-latest | native |
| `x86_64-apple-darwin` | macos-latest | native |
| `x86_64-pc-windows-msvc` | windows-latest | MSVC |

**Actions:**
- Audit `release.yml` for target coverage
- Add `musl` static build for Linux (most portable)
- Add binary size check step: fail if >25MB stripped
- Ensure release assets have consistent naming: `pi-{target}.tar.gz`

### 3b. Self-Update Command (`pi update`)

**What exists:** `src/version_check.rs` — polls GitHub releases, caches
result for 24h, compares versions.

**What to build:**
- `pi update` CLI subcommand (add to `src/cli.rs`)
- Downloads the appropriate release asset for the current platform
- Verifies checksum (SHA256 from release notes or `.sha256` sidecar file)
- Replaces the running binary atomically (rename + exec)
- Falls back gracefully if permissions prevent self-replacement

**Implementation sketch:**
```rust
// cli.rs
#[derive(Parser)]
enum Command {
    Update {
        /// Check for updates without installing
        #[arg(long)]
        check_only: bool,
    },
}
```

### 3c. Shell Completions

**What exists:** `clap_complete` is already a Cargo dependency.

**What to build:**
- `pi completions <shell>` subcommand that prints completion script
- Support: bash, zsh, fish, powershell
- Document installation in README: `pi completions zsh > ~/.zfunc/_pi`

### 3d. Install Script

**What to build:**
- `install.sh` — detects OS/arch, downloads latest release, installs to
  `/usr/local/bin/pi` (or `~/.local/bin/pi` without sudo)
- One-liner: `curl -fsSL https://example.com/install.sh | sh`
- Validates checksum before installing

## Acceptance Criteria

- `release.yml` produces binaries for all 6 targets
- Stripped Linux musl binary is <25MB
- `pi update` downloads and replaces the binary on all platforms
- `pi completions zsh` produces valid zsh completions
- `install.sh` works on macOS (arm64/x64) and Linux (x64/arm64)
