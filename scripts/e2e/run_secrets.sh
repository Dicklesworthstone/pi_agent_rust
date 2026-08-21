#!/usr/bin/env bash
# scripts/e2e/run_secrets.sh — Focused e2e lane for the secrets vault
# (bd-cv653.7.9).
#
# Hermetic: runs the secrets:: unit suite (detector rules matrix, vault
# stability, restore paths, mode FSM, overlap collapse) plus the secrets
# integration target (outbound placeholder canary, inbound write-restore +
# echo mask, block-mode named refusal, off-mode byte identity, export
# masking). Capture-provider stubs only; no network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/secrets/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-secrets-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[secrets] Running secrets:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib secrets:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[secrets] Running secrets integration target (correlation: $CORRELATION_ID)"
cargo test --test secrets -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[secrets] PASS (artifacts: $ARTIFACT_DIR)"
