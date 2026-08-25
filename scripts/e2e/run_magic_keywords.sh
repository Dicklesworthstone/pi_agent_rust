#!/usr/bin/env bash
# scripts/e2e/run_magic_keywords.sh — Focused e2e lane for magic keywords
# (bd-cv653.3.6).
#
# Hermetic: runs the magic_keywords:: unit suite (tokenizer exclusion
# matrix: code spans, fences, XML sections, identifiers, paths,
# punctuation boundaries, settings toggles, custom words) plus the
# magic_keywords integration target (capture-provider and RPC thinking-level
# proof, exactly-once directive injection, settings disable, untouched
# code/path cases, replayable activation telemetry). No network lanes. The
# same integration target is registered in tests/suite_classification.toml,
# so run_all's focused and CI profiles execute it through their normal
# build-first unit-target phase.
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

RCH_BIN="${RCH_BIN:-rch}"
if ! command -v "$RCH_BIN" >/dev/null 2>&1; then
    echo "[magic-keywords] RCH is required but unavailable: $RCH_BIN" >&2
    exit 1
fi

echo "[magic-keywords] Running magic_keywords:: unit suite (correlation: $CORRELATION_ID)"
"$RCH_BIN" exec -- cargo test -j 2 --lib magic_keywords:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[magic-keywords] Running magic_keywords integration target (correlation: $CORRELATION_ID)"
"$RCH_BIN" exec -- cargo test -j 2 --test magic_keywords -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[magic-keywords] PASS (artifacts: $ARTIFACT_DIR)"
