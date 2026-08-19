#!/usr/bin/env bash
# scripts/e2e/run_github_tool.sh — Focused e2e lane for the github tool
# (bd-cv653.2.3).
#
# Hermetic by default: runs the github:: unit suite (remote parsing, card
# formatting, diff truncation, cache TTL, error taxonomy, stub-gh execute —
# the stub test self-skips on hosts whose security tooling stalls exec of
# fresh scripts). Live lane (network + auth) is opt-in via GITHUB_E2E_LIVE=1
# and exercises issue_view against this repository.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/github-tool/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-github-tool-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[github-tool] Running github:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib github:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

if [[ "${GITHUB_E2E_LIVE:-0}" == "1" ]]; then
    echo "[github-tool] LIVE lane: gh auth + issue read against origin repo"
    if ! command -v gh >/dev/null 2>&1; then
        echo "[github-tool] SKIP live: gh not installed" | tee "$ARTIFACT_DIR/live.log"
    elif ! gh auth status >/dev/null 2>&1; then
        echo "[github-tool] SKIP live: gh not authenticated" | tee "$ARTIFACT_DIR/live.log"
    else
        gh issue list --limit 1 --json number,title 2>&1 | tee "$ARTIFACT_DIR/live.log"
        echo "[github-tool] LIVE PASS"
    fi
fi

echo "[github-tool] PASS (artifacts: $ARTIFACT_DIR)"
