#!/usr/bin/env bash
# scripts/e2e/run_jobs.sh — Focused e2e lane for background bash jobs
# (bd-cv653.3.10).
#
# Hermetic: runs the jobs:: unit suite (registry, tail ring, wait/cancel)
# plus the jobs integration target (instant background start + completion
# notice drain, tree-kill cancel with a child-spawning script, session-exit
# kill_all with zero survivors, and the PI_JOBS_AT_CAPACITY ninth-job
# refusal). No network lanes.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/jobs/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-jobs-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[jobs] Running jobs:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib jobs:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[jobs] Running jobs integration target (correlation: $CORRELATION_ID)"
cargo test --test jobs -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[jobs] PASS (artifacts: $ARTIFACT_DIR)"
