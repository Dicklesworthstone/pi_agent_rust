#!/usr/bin/env bash
# scripts/e2e/run_magic_keywords.sh — Focused e2e lane for magic keywords
# (bd-cv653.3.6).
#
# Hermetic: runs the magic_keywords:: unit suite (tokenizer exclusion
# matrix: code spans, fences, XML sections, identifiers, paths,
# punctuation boundaries, settings toggles, custom words) plus the
# magic_keywords integration target (capture-provider thinking-level proof,
# exactly-once directive injection, settings disable, untouched code/path
# cases, activation ledger). No network lanes.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/magic-keywords/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-magic-keywords-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[magic-keywords] Running magic_keywords:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib magic_keywords:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[magic-keywords] Running magic_keywords integration target (correlation: $CORRELATION_ID)"
cargo test --test magic_keywords -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[magic-keywords] PASS (artifacts: $ARTIFACT_DIR)"
