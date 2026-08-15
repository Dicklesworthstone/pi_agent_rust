#!/usr/bin/env bash
# scripts/e2e/run_ast_tools.sh — Focused e2e lane for the ast_grep/ast_edit
# structural tools (bd-cv653.1.3).
#
# Runs tests/e2e_ast_tools.rs: stages a structural codemod on a fixture crate,
# applies it via the resolve lifecycle, and proves the rewritten crate still
# compiles with `cargo check --offline`. No network, no mocks.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/ast-tools/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-ast-tools-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[ast-tools] Running e2e_ast_tools (correlation: $CORRELATION_ID)"
echo "[ast-tools] Artifacts: $ARTIFACT_DIR"

cargo test --test e2e_ast_tools -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/e2e_ast_tools.log"

echo "[ast-tools] PASS (log: $ARTIFACT_DIR/e2e_ast_tools.log)"
