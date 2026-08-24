#!/usr/bin/env bash
# Bounded parallel stress harness for the PiJS host-filesystem policy tests
# (bd-xhl7u).
#
# Drives the lib test binary through escalating interference levels:
#   1. isolated trio iterations (thread-count variance)
#   2. whole extensions_js module at high parallelism
#   3. optional full-suite rounds when FULL_SUITE=1
#
# Every failure is recorded verbatim under ${PIJS_STRESS_DIR:-/tmp/pijs-stress}
# with the round and iteration that produced it, so a transient ordering or
# shared-state defect can be isolated from captured decision events
# (pijs.hostfs.decision / pijs.resolve.ownership_denied) instead of guessed at.
#
# Usage:
#   scripts/stress_pijs_host_flake.sh [isolated_rounds] [module_rounds]
# Env:
#   FULL_SUITE=1      add full-suite rounds (slow; ~10 min each locally)
#   TEST_THREADS_CAP  max per-process test threads (default 8)

set -u

ISOLATED_ROUNDS=${1:-10}
MODULE_ROUNDS=${2:-5}
THREADS_CAP=${TEST_THREADS_CAP:-8}
STRESS_DIR=${PIJS_STRESS_DIR:-/tmp/pijs-stress}

mkdir -p "$STRESS_DIR"
failures="$STRESS_DIR/failures.log"
: >"$failures"

BIN=$(ls -t "${CARGO_TARGET_DIR:-target}/debug/deps/pi-"* 2>/dev/null | grep -v '\.d$' | head -1 || true)
if [[ -z "$BIN" ]]; then
  echo "error: build the lib test binary first (cargo test --lib --no-run)" >&2
  exit 2
fi
echo "using binary: $BIN"

run_and_record() {
  local label=$1 round=$2 iter=$3 threads=$4 filter=$5
  if ! "$BIN" $filter --test-threads="$threads" \
    >"$STRESS_DIR/${label}-r${round}-i${iter}.log" 2>&1; then
    echo "FAIL [$label round=$round iter=$i threads=$threads]" >>"$failures"
  fi
}

echo "== phase 1: isolated pijs_host_ iterations ($ISOLATED_ROUNDS rounds x5)"
for round in $(seq 1 "$ISOLATED_ROUNDS"); do
  for i in 1 2 3 4 5; do
    threads=$(( (i % THREADS_CAP) + 1 ))
    run_and_record isolated "$round" "$i" "$threads" "pijs_host_" &
  done
  wait
done

echo "== phase 2: extensions_js::tests module ($MODULE_ROUNDS rounds)"
for round in $(seq 1 "$MODULE_ROUNDS"); do
  run_and_record module "$round" 1 "$THREADS_CAP" "extensions_js::tests" &
done
wait

if [[ "${FULL_SUITE:-0}" == "1" ]]; then
  echo "== phase 3: full suite rounds"
  for round in 1 2 3; do
    run_and_record full "$round" 1 "$THREADS_CAP" "" &
  done
  wait
fi

total_failures=$(grep -c '^FAIL' "$failures" 2>/dev/null || true)
echo "stress complete: $total_failures failing invocation(s)"
[[ "$total_failures" -eq 0 ]] || {
  echo "see $failures and per-run logs in $STRESS_DIR"
  exit 1
}
