# f0010: Pre-Commit Hooks & Agent Commit Review

> Priority: High | Effort: Done (initial setup)

---

## Problem

AI agents (Claude Code, etc.) should not commit code without human review.
Two layers of defense are needed:

1. **Pre-commit hooks** — automated code quality gates that catch problems
   before any commit lands (agent or human)
2. **Claude Code permissions** — ensure `git commit`, `git push`, `git add`,
   and other mutating git operations require explicit user approval

## Implementation

### Pre-Commit Hooks (`.pre-commit-config.yaml`)

Managed via [prek](https://prek.j178.dev) (pre-commit compatible).

| Hook | Source | What it catches |
|------|--------|-----------------|
| `cargo fmt --check` | doublify/pre-commit-rust | Formatting violations |
| `cargo clippy -D warnings` | doublify/pre-commit-rust | Lint warnings |
| `cargo test --lib` | local | Unit test regressions (~80s) |
| `trailing-whitespace` | pre-commit-hooks | Trailing whitespace (excl .md) |
| `end-of-file-fixer` | pre-commit-hooks | Missing final newline |
| `check-yaml` | pre-commit-hooks | YAML syntax |
| `check-toml` | pre-commit-hooks | TOML syntax |
| `check-merge-conflict` | pre-commit-hooks | Leftover conflict markers |
| `check-added-large-files` | pre-commit-hooks | Files >500KB |
| `detect-private-key` | pre-commit-hooks | SSH/PGP private keys |
| `gitleaks` | gitleaks/gitleaks | Secrets, tokens, API keys |

### Claude Code Permissions (`.claude/settings.local.json`)

**Allowed without prompting** (read-only, safe operations):
- `cargo test`, `cargo check`, `cargo clippy`, `cargo fmt`
- `git status`, `git diff`, `git log`, `git branch`, `git show`
- `git fetch`, `git remote`, `git ls-remote`, `git ls-files`
- `git merge-base`, `git rev-parse`, `git submodule`
- `bd *` (beads issue tracker)
- `ls`, `wc`, `rustc --version`, `cargo --version`

**Requires user approval** (not in allow list):
- `git commit` — user reviews staged changes before approving
- `git push` — user confirms remote target
- `git add` — user confirms which files are staged
- `git rebase`, `git merge` — user confirms strategy
- `git checkout`, `git reset` — user confirms branch/state changes
- Any destructive filesystem operations

### How It Works

1. Agent writes code and runs tests (auto-approved)
2. Agent calls `git add <files>` → **user prompted to approve**
3. Agent calls `git commit -m "..."` → **user prompted to approve**
   - Pre-commit hooks run automatically (fmt, clippy, tests, secrets)
   - If any hook fails, commit is rejected
4. Agent calls `git push` → **user prompted to approve**

The user sees each step and can deny any operation.

## Setup

```bash
# Install pre-commit hooks
prek install --install-hooks

# Run manually on all files
prek run --all-files

# Claude Code permissions are in .claude/settings.local.json
```
