#!/usr/bin/env bash
# scripts/e2e/run_lsp.sh — Focused e2e lane for the lsp code-intelligence
# tool (bd-cv653.1.1).
#
# Runs tests/e2e_lsp.rs: renames a symbol across files in a fixture crate via
# a real rust-analyzer, proves zero dangling references, then proves the
# rewritten crate still compiles with `cargo check --offline`. Skips honestly
# when rust-analyzer is absent. No network, no mocks.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/lsp/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-lsp-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[lsp] Running e2e_lsp (correlation: $CORRELATION_ID)"
echo "[lsp] Artifacts: $ARTIFACT_DIR"

cargo test --test e2e_lsp -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/e2e_lsp.log"

echo "[lsp] PASS (log: $ARTIFACT_DIR/e2e_lsp.log)"
