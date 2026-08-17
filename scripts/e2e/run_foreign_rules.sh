#!/usr/bin/env bash
# scripts/e2e/run_foreign_rules.sh — Focused e2e lane for foreign-format
# workspace rules import (bd-cv653.6.2).
#
# Runs tests/e2e_foreign_rules.rs: a mixed-format fixture workspace (Cursor
# MDC/legacy, Cline, Copilot, Windsurf, Gemini) assembled into the system
# prompt with provenance, scoped-rule activation matrix, and read-only proof
# via mtime assertions. Hermetic; no network, no mocks.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/foreign-rules/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-foreign-rules-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[foreign-rules] Running e2e_foreign_rules (correlation: $CORRELATION_ID)"
echo "[foreign-rules] Artifacts: $ARTIFACT_DIR"

cargo test --test e2e_foreign_rules -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/e2e_foreign_rules.log"

echo "[foreign-rules] PASS (log: $ARTIFACT_DIR/e2e_foreign_rules.log)"
