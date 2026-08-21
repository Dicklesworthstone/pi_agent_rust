#!/usr/bin/env bash
# scripts/e2e/run_worktree_iso.sh — Focused e2e lane for subagent worktree
# isolation (bd-cv653.5.2).
#
# Hermetic: runs the worktree_iso:: unit suite (dirty materialization,
# byte-identical apply, conflict refusal, prefix-only reaper, non-git
# refusal) plus the worktree_iso integration target (two-lane collide with
# serial apply + clean conflict report, worktree CLI reaper, tool-level
# non-git refusal, round-7 dirty-tree invariant). Local git only; no
# network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/worktree-iso/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-worktree-iso-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[worktree-iso] Running worktree_iso:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib worktree_iso:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[worktree-iso] Running worktree_iso integration target (correlation: $CORRELATION_ID)"
cargo test --test worktree_iso -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[worktree-iso] PASS (artifacts: $ARTIFACT_DIR)"
