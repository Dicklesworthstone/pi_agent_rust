#!/usr/bin/env bash
# Focused hermetic E2E lane for /btw and /tan (bd-cv653.3.16).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/btw_tan/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-btw-tan-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[btw-tan] Running focused unit proofs (correlation: $CORRELATION_ID)"
cargo test --lib tan_ -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[btw-tan] Running interactive integration target"
cargo test --test btw_tan -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/e2e.log"

echo "[btw-tan] PASS (artifacts: $ARTIFACT_DIR)"
