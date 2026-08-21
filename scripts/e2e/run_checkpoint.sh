#!/usr/bin/env bash
# scripts/e2e/run_checkpoint.sh — Focused e2e lane for checkpoint/rewind/
# fresh/retry + --max-time (bd-cv653.3.7).
#
# Hermetic: runs the checkpoint:: unit suite (mark/find roundtrip, token
# estimation) plus the checkpoint integration target (mark→20 turns→rewind
# with tree preservation, fresh transcript-identical, retry truncation,
# max-time boundary marker) plus the RPC cycle test in e2e_rpc
# (checkpoint/rewind/fresh/retry commands over the JSON-line protocol).
# Keyless replay providers only; no network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/checkpoint/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-checkpoint-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[checkpoint] Running checkpoint:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib checkpoint:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[checkpoint] Running checkpoint integration target (correlation: $CORRELATION_ID)"
cargo test --test checkpoint -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[checkpoint] Running RPC cycle test (correlation: $CORRELATION_ID)"
cargo test --test e2e_rpc rpc_checkpoint -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/rpc.log"

echo "[checkpoint] PASS (artifacts: $ARTIFACT_DIR)"
