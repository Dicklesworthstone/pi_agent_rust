#!/usr/bin/env bash
# scripts/e2e/run_eval_tool.sh — Focused e2e lane for the eval tool
# (bd-cv653.1.4).
#
# Hermetic: runs the eval:: unit suite — persistent-state cells, trailing
# expression semantics, exception/crash/timeout taxonomy, and the tool
# re-entry bridge (tool.read from inside Python + whitelist denial + path
# policy parity with direct reads). Requires python3; tests self-skip
# honestly when it is absent.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/eval-tool/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-eval-tool-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[eval-tool] Running eval:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib eval:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[eval-tool] PASS (artifacts: $ARTIFACT_DIR)"
