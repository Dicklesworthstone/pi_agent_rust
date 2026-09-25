---
name: pi-agent-rust
description: >-
  Speeds up pi_agent_rust development and verification workflows. Use when editing providers,
  tools, sessions, extensions, installer/uninstaller logic, or triaging regressions in this repo.
---

<!-- pi_agent_rust installer managed skill -->

# Pi Agent Rust

## Use This Skill When

- You are working inside `pi_agent_rust` and need the fastest path to safe, verified edits.
- You are touching provider/tool/session/extension behavior and need targeted triage.
- You are changing installer/uninstaller/skill install behavior and need deterministic safety checks.
- You need symptom-first debugging playbooks instead of ad-hoc command hunting.

## 60-Second Bootstrap

```bash
export CARGO_TARGET_DIR="/data/tmp/pi_agent_rust/${USER:-agent}"
export TMPDIR="/data/tmp/pi_agent_rust/${USER:-agent}/tmp"
mkdir -p "$TMPDIR"

rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings
cargo fmt --check
bash tests/installer_regression.sh
```

## Symptom Router

| Symptom | First 3 Commands |
|---|---|
| Provider stream/tool-call regression | `cargo test --test provider_streaming -- --nocapture` ; `rg -n "stream|tool|delta|event|SSE" src/providers src/sse.rs` ; `cargo test conformance` |
| Session replay/index drift | `cargo test session -- --nocapture` ; `rg -n "Session|save|open|index|jsonl|sqlite" src/session.rs src/session_index.rs` ; `cargo test conformance` |
| Extension policy/runtime failure | `cargo test extension -- --nocapture` ; `rg -n "policy|hostcall|capability|quickjs|deny|allow" src/extensions.rs src/extensions_js.rs` ; `cargo test conformance` |
| Installer/uninstaller/skill issue | `bash tests/installer_regression.sh` ; `rg -n "AGENT_SKILL_STATUS|CHECKSUM_STATUS|SIGSTORE_STATUS|COMPLETIONS_STATUS" install.sh` ; `rg -n "managed skill|expected skill directory|PIAR_AGENT_SKILL" uninstall.sh` |
| Interactive vs RPC divergence | `cargo test --test e2e_rpc -- --nocapture` ; `rg -n "interactive|rpc|stdin|event|session" src/main.rs src/interactive.rs src/rpc.rs` ; `cargo test conformance` |

For deeper diagnosis, use `references/DEBUGGING-PLAYBOOKS.md`.

## Non-Negotiables

- Read `AGENTS.md` first, then follow it exactly.
- Do not delete files or run destructive git/filesystem commands.
- Keep edits in-place; avoid creating variant files for the same purpose.
- Use `main` semantics in docs/scripts; do not introduce `master`.
- Prefer `rg` for fast text recon and `ast-grep` for structural matching/refactors.
- Prefer `rch exec -- <cargo ...>` for heavy compile/test workloads.
- After substantive edits, run compile/lint/format gates and the smallest relevant regression slice.

## Core Workflow

- [ ] Recon: identify exact change surface and invariants.
- [ ] Implement: minimal, behavior-focused patch with explicit failure semantics.
- [ ] Validate: targeted tests first, broaden only as needed.
- [ ] Verify UX: error/status output is explicit, stable, and non-ambiguous.
- [ ] Sync docs: update `README.md` when flags/behavior/user guidance changed.

## Changed Files -> Required Tests

| Changed Files (examples) | Minimum Required Tests |
|---|---|
| `install.sh`, `uninstall.sh`, `.claude/skills/pi-agent-rust/**` | `bash -n install.sh uninstall.sh tests/installer_regression.sh` ; `shellcheck -x install.sh uninstall.sh tests/installer_regression.sh` ; `bash tests/installer_regression.sh` ; `bash scripts/skill-smoke.sh` |
| `src/providers/**`, `src/provider.rs`, `src/sse.rs` | `cargo test --test provider_streaming` ; `cargo test --test e2e_provider_streaming` ; `cargo test conformance` |
| `src/session.rs`, `src/session_index.rs`, `src/session_test.rs` | `cargo test session` ; `cargo test conformance` |
| `src/extensions.rs`, `src/extensions_js.rs` | `cargo test extension` ; `cargo test conformance` |
| `src/tools.rs` | `cargo test tools` ; `cargo test conformance` |
| `src/interactive.rs`, `src/rpc.rs`, `src/main.rs` | `cargo test --test e2e_rpc` ; `cargo test conformance` |

## Do Not Run Yet

Run these only after targeted repro + focused slice indicates need:

- Broad `cargo test` across entire workspace when a narrower slice already reproduces.
- Heavy multi-surface runs before confirming changed-file impact.
- Repeated full conformance loops while the core failing slice is still unstable.

## High-Value Commands

```bash
# Fast recon
git status --short
rg -n "install|uninstall|skill|checksum|sigstore|completion|provider|session|extension" \
  install.sh uninstall.sh README.md tests/installer_regression.sh src/

# Installer + skill safety gates
bash -n install.sh uninstall.sh tests/installer_regression.sh
shellcheck -x install.sh uninstall.sh tests/installer_regression.sh
bash tests/installer_regression.sh
bash scripts/skill-smoke.sh

# Rust gates
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

For an expanded command cookbook, see `references/COMMANDS.md`.
For deep incident triage, see `references/DEBUGGING-PLAYBOOKS.md`.

## Critical Files

- `src/main.rs`: CLI entry and mode dispatch.
- `src/agent.rs`: agent loop and tool iteration behavior.
- `src/provider.rs`: provider trait contract.
- `src/providers/`: provider implementations and factory wiring.
- `src/tools.rs`: built-in tools (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`).
- `src/session.rs`: JSONL session persistence.
- `src/session_index.rs`: session index and metadata cache.
- `src/extensions.rs` + `src/extensions_js.rs`: extension policy and QuickJS bridge.
- `src/interactive.rs` + `src/rpc.rs`: TUI and RPC/stdin surfaces.
- `install.sh` + `uninstall.sh`: install lifecycle, migration, and skill management.
- `tests/installer_regression.sh`: installer regression harness.
- `scripts/skill-smoke.sh`: skill integrity + inline-sync validation.

## Known Footguns

- Custom artifact install paths without compatible release context can fall back incorrectly if not explicitly guarded.
- Skill status can become misleading on mixed outcomes unless partial/failure branches are explicit.
- Uninstall logic must enforce both marker checks and expected destination path shape.
- Installer progress/status text should stay on stderr when stdout is used for data plumbing.
- Bundled skill and inline fallback can silently drift unless explicitly checked.

## Patch Patterns

### Pattern 1: Mixed Outcome Status Clarity

```bash
# BEFORE: everything collapsed into "skipped custom"
if [ "$skipped_custom" -ge 1 ]; then
  AGENT_SKILL_STATUS="skipped (existing custom skill)"
fi

# AFTER: distinguish custom-skip from write failure
if [ "$skipped_custom" -ge 1 ] && [ "$failed_writes" -ge 1 ]; then
  AGENT_SKILL_STATUS="partial (custom skill kept; other install failed)"
elif [ "$skipped_custom" -ge 1 ]; then
  AGENT_SKILL_STATUS="skipped (existing custom skill)"
fi
```

### Pattern 2: Safe Skill Replacement

```bash
# BEFORE: remove destination before validating copy result
rm -rf "$destination"
cp "$source" "$destination/SKILL.md"

# AFTER: stage then atomically move into place
staged="$(mktemp -d ...)"
cp "$source" "$staged/SKILL.md"
mv "$staged" "$destination"
```

## Failure Triage

- Installer summary/status mismatch:
  trace `AGENT_SKILL_STATUS`, `CHECKSUM_STATUS`, and `COMPLETIONS_STATUS` in `install.sh`.
- Install/uninstall safety concern:
  verify marker checks and expected destination guards in both scripts.
- Provider/session/extension regressions:
  use symptom router, then follow `references/DEBUGGING-PLAYBOOKS.md`.
- Docs drift:
  ensure `README.md` flags/examples match current installer behavior.

## Done Criteria

- Changed-file matrix minimum tests passed.
- Compile/lint/format checks passed for touched surfaces.
- Installer/skill changes pass `tests/installer_regression.sh` and `scripts/skill-smoke.sh`.
- Behavior is explicit on failure paths; no silent fallback surprises.
- Skill docs and inline fallback remain aligned and current.
