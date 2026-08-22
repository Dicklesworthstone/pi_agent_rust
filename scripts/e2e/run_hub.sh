#!/usr/bin/env bash
# scripts/e2e/run_hub.sh — Focused e2e lane for hub process supervision
# (bd-cv653.5.4).
#
# Hermetic: runs the hub:: unit suite (ring cursors, readiness gates,
# REPL send, stop/restart) plus the hub integration target (fixture HTTP
# server with log+port readiness conjunction, PTY REPL drive through the
# tool surface, duplicate-name/restart flow, session-exit zero survivors,
# and the hub jobs action group wrapping the background-jobs registry).
# The HTTP fixture binds 127.0.0.1 only; no external network.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/hub/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-hub-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[hub] Running hub:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib hub:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[hub] Running hub integration target (correlation: $CORRELATION_ID)"
cargo test --test hub -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/integration.log"

echo "[hub] Running agent-hub registry suite (bd-cv653.5.3; correlation: $CORRELATION_ID)"
cargo test --lib agent_hub -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/agent_hub_units.log"
cargo test --lib hub_agent -- --nocapture 2>&1 | tee -a "$ARTIFACT_DIR/agent_hub_units.log"

echo "[hub] Running agent-hub integration target (correlation: $CORRELATION_ID)"
cargo test --test agent_hub -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/agent_hub_integration.log"

echo "[hub] PASS (artifacts: $ARTIFACT_DIR)"
