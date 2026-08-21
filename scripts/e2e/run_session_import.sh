#!/usr/bin/env bash
# scripts/e2e/run_session_import.sh — Focused e2e lane for foreign session
# import (bd-cv653.6.4).
#
# Hermetic: runs the session_import:: unit suite (Claude/Codex fixture
# goldens, idempotency, corruption tolerance) plus the session_import
# integration target (full-fidelity import with tool-pair preservation,
# reasoning-as-thinking, idempotent re-import, openable via the native
# loader). Fixture files only; no network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/session-import/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-session-import-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[session-import] Running session_import:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib session_import:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[session-import] Running session_import integration target (correlation: $CORRELATION_ID)"
cargo test --test session_import -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[session-import] PASS (artifacts: $ARTIFACT_DIR)"
