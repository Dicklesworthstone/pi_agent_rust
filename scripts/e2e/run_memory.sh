#!/usr/bin/env bash
# scripts/e2e/run_memory.sh — Focused e2e lane for the memory bank
# (bd-cv653.4.1).
#
# Hermetic: runs the memory:: unit suite (store CRUD, FTS ranking, dedupe,
# redaction, tombstones, project isolation, mental-model budget) plus the
# memory integration target (tool-surface redaction, backend gate, reflect
# with a stub provider citing ids, cross-instance persistence, startup
# mental-model injection) and the FTS5 capability spike. No network lanes.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/memory/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-memory-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[memory] Running FTS5 spike (correlation: $CORRELATION_ID)"
cargo test --test fts5_spike -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/spike.log"

echo "[memory] Running memory:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib memory:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[memory] Running memory integration target (correlation: $CORRELATION_ID)"
cargo test --test memory -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[memory] PASS (artifacts: $ARTIFACT_DIR)"
