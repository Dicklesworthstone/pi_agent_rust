#!/usr/bin/env bash
# scripts/e2e/run_workspace.sh — Focused e2e lane for multi-root workspace
# confinement (bd-cv653.3.12).
#
# Hermetic: runs the workspace:: unit suite (root-set FSM, canonicalization,
# immediate-revocation sharing, identity-default legacy behavior), the
# two-root read acceptance test (read spans additional roots; outside all
# roots fails closed; remove_root revokes immediately), and asserts the
# --add-dir flag is wired into the CLI surface. No network lanes.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/workspace/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-workspace-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[workspace] Running workspace:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib workspace:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[workspace] Running two-root acceptance test (correlation: $CORRELATION_ID)"
cargo test --lib read_tool_spans_additional_roots_and_revokes_on_removal -- \
  --nocapture 2>&1 | tee "$ARTIFACT_DIR/acceptance.log"

echo "[workspace] Verifying --add-dir CLI surface (correlation: $CORRELATION_ID)"
cargo run --bin pi --quiet -- --help 2>&1 | tee "$ARTIFACT_DIR/cli_help.log" | grep -q -- "--add-dir" || {
  echo "[workspace] FAIL: --add-dir missing from CLI help" >&2
  exit 1
}

echo "[workspace] PASS (artifacts: $ARTIFACT_DIR)"
