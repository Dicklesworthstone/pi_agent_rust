#!/usr/bin/env bash
# scripts/e2e/run_token_count.sh — Focused e2e lane for BPE token counting
# (bd-cv653.7.1).
#
# Hermetic: runs the token_count:: unit suite (table selection, reference
# vectors, 1MB throughput evidence) plus the compaction estimate fixtures
# (BPE reference values on the admission path). `pi token` prints per-table
# counts. No network lanes.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/token-count/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-token-count-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[token-count] Running token_count:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib token_count:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[token-count] Running compaction estimate fixtures (correlation: $CORRELATION_ID)"
cargo test --lib compaction::tests::estimate -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/estimates.log"

echo "[token-count] pi token utility (correlation: $CORRELATION_ID)"
cargo run --bin pi --quiet -- token "hello world this is a test" 2>&1 | tee "$ARTIFACT_DIR/pi_token.log"

echo "[token-count] PASS (artifacts: $ARTIFACT_DIR)"
