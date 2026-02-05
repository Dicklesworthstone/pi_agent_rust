#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! command -v hyperfine >/dev/null 2>&1; then
  echo "hyperfine not installed; install with: cargo install hyperfine" >&2
  exit 1
fi

cargo build --release --bin pijs_workload

BIN="$ROOT/target/release/pijs_workload"
ITERATIONS="${ITERATIONS:-200}"
TOOL_CALLS="${TOOL_CALLS:-1}"

hyperfine --warmup 3 --runs 10 "$BIN --iterations ${ITERATIONS} --tool-calls ${TOOL_CALLS}"
