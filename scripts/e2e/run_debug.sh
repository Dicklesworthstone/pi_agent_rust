#!/usr/bin/env bash
# scripts/e2e/run_debug.sh — Focused e2e lane for the debug tool (bd-cv653.1.2).
#
# Runs tests/debug.rs: the debugpy lane proves the full acceptance round
# trip (launch → entry stop → function breakpoint → continue → stack /
# scopes / variables / step / evaluate → terminate with zero leftover
# processes), the lldb-dap attach lane proves attach-by-pid, and the
# lldb-dap launch quirk lane stays #[ignore]d pending an adapter upgrade.
# PI_DEBUG_REQUIRE_LLDB=1 turns adapter-absence skips into loud failures.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/debug/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-debug-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[debug] Running e2e debug lanes (correlation: $CORRELATION_ID)"
echo "[debug] Artifacts: $ARTIFACT_DIR"

cargo test --test debug -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/e2e_debug.log"

echo "[debug] PASS (log: $ARTIFACT_DIR/e2e_debug.log)"
