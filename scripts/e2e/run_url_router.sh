#!/usr/bin/env bash
# scripts/e2e/run_url_router.sh — Focused e2e lane for the internal URL
# scheme router (bd-cv653.6.3).
#
# Hermetic: runs the url_router:: unit suite (scheme parsing, conflict
# resolution matrix, local scratch, gh reference parsing) plus the
# url_router integration target (skill read parity with the loader,
# conflict read/write/bulk through the tool, unknown-scheme error shape,
# stubbed-gh pr:// backend, pagination contract). gh is stubbed via
# ResolveOptions; no network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/url-router/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-url-router-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[url-router] Running url_router:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib url_router:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[url-router] Running url_router integration target (correlation: $CORRELATION_ID)"
cargo test --test url_router -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[url-router] PASS (artifacts: $ARTIFACT_DIR)"
