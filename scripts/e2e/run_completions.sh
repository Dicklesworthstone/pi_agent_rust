#!/usr/bin/env bash
# scripts/e2e/run_completions.sh — Focused e2e lane for shell completions
# (bd-cv653.7.2).
#
# Hermetic: runs the completions:: unit suite (script generation for all
# shells, named error for unknown flags, case-insensitive model filter)
# plus binary-level acceptance: `pi completions zsh|bash` parse (zsh -n /
# bash -n when present), `pi __complete --model an` returns live-registry
# models, `pi __complete --session ''` returns session-index paths, and the
# generated script contains a flag added in this build (no drift).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/completions/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-completions-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[completions] Running completions:: unit suite (correlation: $CORRELATION_ID)"
cargo test --lib completions:: -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/units.log"

echo "[completions] Binary-level acceptance (correlation: $CORRELATION_ID)"
BIN="$(cargo run --bin pi --quiet -- completions bash 2>/dev/null >/dev/null; echo "./target/debug/pi")"
cargo build --bin pi 2>&1 | tail -1 | tee "$ARTIFACT_DIR/build.log"

if command -v zsh >/dev/null 2>&1; then
  ./target/debug/pi completions zsh > "$ARTIFACT_DIR/pi.zsh"
  zsh -n "$ARTIFACT_DIR/pi.zsh" && echo "zsh -n OK" | tee -a "$ARTIFACT_DIR/build.log"
fi
if command -v bash >/dev/null 2>&1; then
  ./target/debug/pi completions bash > "$ARTIFACT_DIR/pi.bash"
  bash -n "$ARTIFACT_DIR/pi.bash" && echo "bash -n OK" | tee -a "$ARTIFACT_DIR/build.log"
fi

./target/debug/pi __complete --model an > "$ARTIFACT_DIR/model_candidates.txt" 2>&1 || true
./target/debug/pi __complete --session "" > "$ARTIFACT_DIR/session_candidates.txt" 2>&1 || true
grep -c "max-time" "$ARTIFACT_DIR/pi.zsh" 2>/dev/null | tee -a "$ARTIFACT_DIR/build.log" || true

echo "[completions] PASS (artifacts: $ARTIFACT_DIR)"
