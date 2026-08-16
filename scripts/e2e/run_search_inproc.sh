#!/usr/bin/env bash
# scripts/e2e/run_search_inproc.sh — Focused e2e lane for the in-process
# grep/find search backends (bd-cv653.1.5).
#
# Proves the default `search_backend: inproc` needs NO external binaries:
# builds the lib test binary with the normal toolchain PATH, then executes the
# grep/find/ls tool suites with PATH pointing at an empty directory, so any
# attempt to spawn `rg`/`fd` (or anything else) fails loudly.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/search-inproc/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-search-inproc-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

echo "[search-inproc] Building lib test binary (correlation: $CORRELATION_ID)"
cargo test --lib --no-run 2>&1 | tee "$ARTIFACT_DIR/build.log"

# Locate the freshly built lib test binary from the build log's
# "Executable unittests" line; fall back to cargo re-invocation if absent.
TEST_BIN="$(sed -n 's/.*Executable unittests[^(]*(\(.*\))/\1/p' "$ARTIFACT_DIR/build.log" | tail -1)"
if [[ -z "$TEST_BIN" || ! -x "$TEST_BIN" ]]; then
    echo "[search-inproc] Could not locate lib test binary from build log" >&2
    exit 1
fi

EMPTY_PATH_DIR="$(mktemp -d)"
trap 'rmdir "$EMPTY_PATH_DIR" 2>/dev/null || true' EXIT

echo "[search-inproc] Running grep/find/ls suites with scrubbed PATH=$EMPTY_PATH_DIR"
PATH="$EMPTY_PATH_DIR" "$TEST_BIN" \
    tools::tests::test_grep_ tools::tests::test_find_ tools::tests::test_ls_ \
    2>&1 | tee "$ARTIFACT_DIR/scrubbed_path_suites.log"

grep -E "test result: ok\." "$ARTIFACT_DIR/scrubbed_path_suites.log" >/dev/null || {
    echo "[search-inproc] FAIL: suites did not pass under scrubbed PATH" >&2
    exit 1
}

echo "[search-inproc] PASS (logs: $ARTIFACT_DIR)"
