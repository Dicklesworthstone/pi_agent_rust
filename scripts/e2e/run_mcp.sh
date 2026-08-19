#!/usr/bin/env bash
# scripts/e2e/run_mcp.sh — Focused e2e lane for the MCP client (bd-cv653.6.1).
#
# Runs tests/mcp.rs with the internal-mcp-fixture feature: a real stdio
# JSON-RPC fixture server proves trust → mount → round-trip → crash/restart,
# and the loopback mock proves the streamable-HTTP transport. The marker env
# var below makes the env-allowlist proof strict (the fixture must never see
# ambient secrets). No network, no mocks-of-the-system-under-test.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/mcp/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-mcp-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

# Secret-marker for the strict env-allowlist proof: the ambient test process
# has it; the spawned fixture server must NOT inherit it.
export PI_MCP_SECRET_MARKER="mcp-secret-marker-$STAMP"

echo "[mcp] Running e2e mcp lanes (correlation: $CORRELATION_ID)"
echo "[mcp] Artifacts: $ARTIFACT_DIR"

cargo test --features internal-mcp-fixture --test mcp -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/e2e_mcp.log"

echo "[mcp] PASS (log: $ARTIFACT_DIR/e2e_mcp.log)"
