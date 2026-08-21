#!/usr/bin/env bash
# scripts/e2e/run_security_scan.sh — Focused e2e lane for the agent-facing
# security scanner (bd-cv653.2.6).
#
# Hermetic: rule-pack parsing, scanner, SARIF v2.1.0 emit/parse round trip,
# disposition suppression, compare deltas, and the self-scan zero-hit guard
# over this repo's src/. No network (OSV lookup is offline-gated in v1).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/security_scan/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-security_scan-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[security_scan] Running unit suite (correlation: $CORRELATION_ID)"
cargo test --lib security_scan -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[security_scan] Running integration target (correlation: $CORRELATION_ID)"
cargo test --test security_scan -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[security_scan] PASS (artifacts: $ARTIFACT_DIR)"
