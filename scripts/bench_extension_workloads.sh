#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if ! command -v hyperfine >/dev/null 2>&1; then
  echo "hyperfine not installed; install with: cargo install hyperfine" >&2
  exit 1
fi

BENCH_CARGO_PROFILE="${BENCH_CARGO_PROFILE:-perf}"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BENCH_ALLOCATORS_CSV="${BENCH_ALLOCATORS_CSV:-system,jemalloc}"
BENCH_ALLOCATOR_FALLBACK="${BENCH_ALLOCATOR_FALLBACK:-system}"
BENCH_CARGO_RUNNER="${BENCH_CARGO_RUNNER:-auto}"
BENCH_SYSTEM_FEATURES="${BENCH_SYSTEM_FEATURES:-image-resize,clipboard,wasm-host,sqlite-sessions}"
BENCH_RUN_ID="${BENCH_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
BENCH_CORRELATION_ID="${BENCH_CORRELATION_ID:-$BENCH_RUN_ID}"

if [[ -z "$BENCH_RUN_ID" || "$BENCH_RUN_ID" != "$BENCH_CORRELATION_ID" ]]; then
  echo "BENCH_RUN_ID and BENCH_CORRELATION_ID must be the same non-empty identifier" >&2
  exit 1
fi
export PI_BENCH_RUN_ID="$BENCH_RUN_ID"
export PI_BENCH_CORRELATION_ID="$BENCH_CORRELATION_ID"

if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
  BENCH_PROFILE_DIR="$TARGET_DIR/$CARGO_BUILD_TARGET/$BENCH_CARGO_PROFILE"
else
  BENCH_PROFILE_DIR="$TARGET_DIR/$BENCH_CARGO_PROFILE"
fi
BIN="$BENCH_PROFILE_DIR/examples/pijs_workload"
ITERATIONS="${ITERATIONS:-2000}"
TOOL_CALLS_CSV="${TOOL_CALLS_CSV:-1,10}"
HYPERFINE_WARMUP="${HYPERFINE_WARMUP:-3}"
HYPERFINE_RUNS="${HYPERFINE_RUNS:-10}"
OUT_DIR="${OUT_DIR:-$TARGET_DIR/perf/$BENCH_CARGO_PROFILE}"
JSONL_OUT="${JSONL_OUT:-$OUT_DIR/pijs_workload_${BENCH_CARGO_PROFILE}.jsonl}"
BENCH_PGO_MODE="${BENCH_PGO_MODE:-off}" # off|train|use|compare
BENCH_PGO_ALLOW_FALLBACK="${BENCH_PGO_ALLOW_FALLBACK:-1}"
BENCH_PGO_PROFILE_DIR="${BENCH_PGO_PROFILE_DIR:-$OUT_DIR/pgo_profile}"
BENCH_PGO_PROFILE_DATA="${BENCH_PGO_PROFILE_DATA:-$BENCH_PGO_PROFILE_DIR/pijs_workload.profdata}"
BENCH_PGO_TRAIN_ITERATIONS="${BENCH_PGO_TRAIN_ITERATIONS:-200}"
BENCH_PGO_TRAIN_TOOL_CALLS="${BENCH_PGO_TRAIN_TOOL_CALLS:-10}"
BENCH_PGO_EVENTS_JSONL="${BENCH_PGO_EVENTS_JSONL:-$OUT_DIR/pgo_pipeline_events.jsonl}"
BENCH_ALLOCATOR_SUMMARY_JSON="${BENCH_ALLOCATOR_SUMMARY_JSON:-$OUT_DIR/allocator_strategy_summary.json}"

mkdir -p "$OUT_DIR"
: > "$JSONL_OUT"
: > "$BENCH_PGO_EVENTS_JSONL"

lower() {
  printf '%s' "$1" | tr '[:upper:]' '[:lower:]'
}

BENCH_PGO_MODE="$(lower "$BENCH_PGO_MODE")"
if [[ "$BENCH_PGO_MODE" != "off" && "$BENCH_PGO_MODE" != "train" && "$BENCH_PGO_MODE" != "use" && "$BENCH_PGO_MODE" != "compare" ]]; then
  echo "warning: unknown BENCH_PGO_MODE '$BENCH_PGO_MODE'; forcing 'off'" >&2
  BENCH_PGO_MODE="off"
fi

BENCH_CARGO_RUNNER="$(lower "$BENCH_CARGO_RUNNER")"
if [[ "$BENCH_CARGO_RUNNER" != "auto" && "$BENCH_CARGO_RUNNER" != "rch" && "$BENCH_CARGO_RUNNER" != "local" ]]; then
  echo "warning: unknown BENCH_CARGO_RUNNER '$BENCH_CARGO_RUNNER'; forcing 'auto'" >&2
  BENCH_CARGO_RUNNER="auto"
fi

CARGO_EXEC_PREFIX=("cargo")
if [[ "$BENCH_CARGO_RUNNER" == "rch" ]]; then
  CARGO_EXEC_PREFIX=("rch" "exec" "--" "cargo")
elif [[ "$BENCH_CARGO_RUNNER" == "auto" ]] && command -v rch >/dev/null 2>&1; then
  CARGO_EXEC_PREFIX=("rch" "exec" "--" "cargo")
fi

run_cargo() {
  "${CARGO_EXEC_PREFIX[@]}" "$@"
}

run_cargo_with_rustflags() {
  local rustflags="$1"
  shift
  if [[ -n "$rustflags" ]]; then
    RUSTFLAGS="$rustflags" "${CARGO_EXEC_PREFIX[@]}" "$@"
  else
    "${CARGO_EXEC_PREFIX[@]}" "$@"
  fi
}

target_supports_jemalloc() {
  local target_args=()
  if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
    target_args=(--target "$CARGO_BUILD_TARGET")
  fi

  local cfg
  if ! cfg="$(rustc --print cfg "${target_args[@]}" 2>/dev/null)"; then
    return 1
  fi

  grep -Eq '^target_os="(linux|macos)"$' <<<"$cfg"
}

build_system_allocator_binary() {
  local rustflags="$1"
  local build_log="$2"
  local args=(build --profile "$BENCH_CARGO_PROFILE" --no-default-features --example pijs_workload)

  if [[ -n "$BENCH_SYSTEM_FEATURES" ]]; then
    args+=(--features "$BENCH_SYSTEM_FEATURES")
  fi

  run_cargo_with_rustflags "$rustflags" "${args[@]}" >>"$build_log" 2>&1
}

build_jemalloc_binary() {
  local rustflags="$1"
  local build_log="$2"
  local features="jemalloc"
  if [[ -n "$BENCH_SYSTEM_FEATURES" ]]; then
    features="$BENCH_SYSTEM_FEATURES,jemalloc"
  fi
  local args=(
    build --profile "$BENCH_CARGO_PROFILE" --no-default-features
    --features "$features" --example pijs_workload
  )
  run_cargo_with_rustflags "$rustflags" "${args[@]}" >>"$build_log" 2>&1
}

find_llvm_profdata() {
  if command -v llvm-profdata >/dev/null 2>&1; then
    command -v llvm-profdata
    return 0
  fi
  if command -v rustup >/dev/null 2>&1; then
    local rustup_profdata
    rustup_profdata="$(rustup which llvm-profdata 2>/dev/null || true)"
    if [[ -n "$rustup_profdata" && -x "$rustup_profdata" ]]; then
      printf '%s\n' "$rustup_profdata"
      return 0
    fi
  fi
  return 1
}

emit_pgo_event() {
  local phase="$1"
  local allocator_requested="$2"
  local allocator_effective="$3"
  local mode_effective="$4"
  local profile_state="$5"
  local fallback_reason="$6"
  local build_log="$7"
  local comparison_json="$8"

  python3 - "$BENCH_PGO_EVENTS_JSONL" "$phase" "$allocator_requested" "$allocator_effective" "$BENCH_PGO_MODE" "$mode_effective" "$profile_state" "$fallback_reason" "$BENCH_PGO_PROFILE_DATA" "$build_log" "$comparison_json" "$BENCH_CARGO_PROFILE" <<'PYEOF'
import datetime
import json
import os
import sys

(
    out_path,
    phase,
    allocator_requested,
    allocator_effective,
    mode_requested,
    mode_effective,
    profile_state,
    fallback_reason,
    profile_data_path,
    build_log,
    comparison_json,
    build_profile,
) = sys.argv[1:]

record = {
    "schema": "pi.perf.pgo_pipeline_event.v1",
    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "phase": phase,
    "build_profile": build_profile,
    "pgo_mode_requested": mode_requested,
    "pgo_mode_effective": mode_effective,
    "profile_data_path": profile_data_path,
    "profile_data_state": profile_state,
    "allocator_requested": allocator_requested,
    "allocator_effective": allocator_effective,
}
if fallback_reason:
    record["fallback_reason"] = fallback_reason
if build_log:
    record["build_log"] = build_log
if comparison_json:
    record["comparison_json"] = comparison_json

os.makedirs(os.path.dirname(out_path), exist_ok=True)
with open(out_path, "a", encoding="utf-8") as handle:
    handle.write(json.dumps(record, separators=(",", ":")) + "\n")
PYEOF
}

build_binary_for_allocator() {
  local allocator_request="$1"
  local rustflags="$2"
  local build_log="$3"

  local normalized
  normalized="$(lower "$allocator_request")"

  EFFECTIVE_ALLOCATOR="system"
  ALLOCATOR_FALLBACK_REASON=""

  mkdir -p "$(dirname "$build_log")"
  : > "$build_log"

  if [[ "$normalized" == "system" ]]; then
    build_system_allocator_binary "$rustflags" "$build_log"
    EFFECTIVE_ALLOCATOR="system"
    return 0
  fi

  if [[ "$normalized" == "jemalloc" ]]; then
    if build_jemalloc_binary "$rustflags" "$build_log"; then
      if target_supports_jemalloc; then
        EFFECTIVE_ALLOCATOR="jemalloc"
      else
        EFFECTIVE_ALLOCATOR="system"
        ALLOCATOR_FALLBACK_REASON="jemalloc_unsupported_target"
      fi
      return 0
    fi
    if [[ "$BENCH_ALLOCATOR_FALLBACK" == "system" ]]; then
      build_system_allocator_binary "$rustflags" "$build_log"
      EFFECTIVE_ALLOCATOR="system"
      ALLOCATOR_FALLBACK_REASON="jemalloc_build_failed"
      return 0
    fi
    return 1
  fi

  # auto mode: try jemalloc first, then fail-closed to system.
  if ! target_supports_jemalloc; then
    build_system_allocator_binary "$rustflags" "$build_log"
    EFFECTIVE_ALLOCATOR="system"
    ALLOCATOR_FALLBACK_REASON="auto_jemalloc_unsupported_target"
    return 0
  fi
  if build_jemalloc_binary "$rustflags" "$build_log"; then
    EFFECTIVE_ALLOCATOR="jemalloc"
    return 0
  fi
  build_system_allocator_binary "$rustflags" "$build_log"
  EFFECTIVE_ALLOCATOR="system"
  ALLOCATOR_FALLBACK_REASON="auto_jemalloc_build_failed"
  return 0
}

build_variant_binary() {
  local allocator_request="$1"
  local rustflags="$2"
  local variant="$3"
  local build_log="$4"
  local variant_bin="$5"

  if ! build_binary_for_allocator "$allocator_request" "$rustflags" "$build_log"; then
    return 1
  fi
  mkdir -p "$(dirname "$variant_bin")"
  cp "$BIN" "$variant_bin"
  chmod +x "$variant_bin"
  return 0
}

write_pgo_delta_json() {
  local baseline_json="$1"
  local pgo_json="$2"
  local out_json="$3"
  local allocator_requested="$4"
  local allocator_effective="$5"
  local tool_calls="$6"
  local pgo_effective_mode="$7"
  local fallback_reason="$8"

  python3 - "$baseline_json" "$pgo_json" "$out_json" "$BENCH_CARGO_PROFILE" "$allocator_requested" "$allocator_effective" "$tool_calls" "$pgo_effective_mode" "$fallback_reason" "$BENCH_PGO_PROFILE_DATA" <<'PYEOF'
import datetime
import json
import math
import pathlib
import sys

(
    baseline_path,
    pgo_path,
    out_path,
    build_profile,
    allocator_requested,
    allocator_effective,
    tool_calls,
    pgo_mode_effective,
    fallback_reason,
    profile_data_path,
) = sys.argv[1:]

def load_mean(path: str) -> float:
    data = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    results = data.get("results") or []
    if not results:
        return float("nan")
    return float(results[0].get("mean", "nan"))

baseline_mean = load_mean(baseline_path)
pgo_mean = load_mean(pgo_path)
if math.isfinite(baseline_mean) and baseline_mean > 0 and math.isfinite(pgo_mean):
    delta_pct = ((baseline_mean - pgo_mean) / baseline_mean) * 100.0
else:
    delta_pct = None

payload = {
    "schema": "pi.perf.pgo_comparison.v1",
    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "build_profile": build_profile,
    "allocator_requested": allocator_requested,
    "allocator_effective": allocator_effective,
    "tool_calls_per_iteration": int(tool_calls),
    "pgo_mode_effective": pgo_mode_effective,
    "profile_data_path": profile_data_path,
    "baseline_hyperfine_json": baseline_path,
    "pgo_hyperfine_json": pgo_path,
    "baseline_mean_seconds": baseline_mean,
    "pgo_mean_seconds": pgo_mean,
    "delta_pct": delta_pct,
}
if fallback_reason:
    payload["fallback_reason"] = fallback_reason

pathlib.Path(out_path).parent.mkdir(parents=True, exist_ok=True)
pathlib.Path(out_path).write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PYEOF
}

emit_allocator_strategy_summary() {
  python3 - "$OUT_DIR" "$JSONL_OUT" "$BENCH_CARGO_PROFILE" "$BENCH_ALLOCATORS_CSV" "$BENCH_ALLOCATOR_FALLBACK" "$BENCH_ALLOCATOR_SUMMARY_JSON" "$BENCH_PGO_EVENTS_JSONL" <<'PYEOF'
import datetime
import glob
import json
import math
import os
import pathlib
import re
import sys

(
    out_dir,
    jsonl_path,
    build_profile,
    allocators_csv,
    fallback_policy,
    out_path,
    events_path,
) = sys.argv[1:]

pattern = re.compile(
    r"hyperfine_pijs_workload_(?P<iterations>\d+)x(?P<tool_calls>\d+)_(?P<profile>[^_]+)_(?P<requested>[^_]+)(?:_(?P<variant>.+?))?_effective-(?P<effective>[^.]+)\.json$"
)

matrix = []
for path in sorted(glob.glob(os.path.join(out_dir, "hyperfine_pijs_workload_*_effective-*.json"))):
    name = os.path.basename(path)
    match = pattern.match(name)
    if match is None:
        continue
    try:
        payload = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        continue

    results = payload.get("results") or []
    mean_value = None
    if results:
        raw_mean = results[0].get("mean")
        if isinstance(raw_mean, (int, float)) and math.isfinite(float(raw_mean)):
            mean_value = float(raw_mean)

    matrix.append(
        {
            "path": path,
            "iterations": int(match.group("iterations")),
            "tool_calls_per_iteration": int(match.group("tool_calls")),
            "build_profile": match.group("profile"),
            "allocator_requested": match.group("requested"),
            "allocator_effective": match.group("effective"),
            "variant": match.group("variant") or "default",
            "mean_seconds": mean_value,
        }
    )

baseline_rows = [
    row
    for row in matrix
    if row["variant"] in {"default", "baseline"}
    and isinstance(row["mean_seconds"], (int, float))
]
if not baseline_rows:
    baseline_rows = [row for row in matrix if isinstance(row["mean_seconds"], (int, float))]

allocator_stats = {}
for row in baseline_rows:
    allocator_stats.setdefault(row["allocator_effective"], []).append(row["mean_seconds"])

aggregates = {}
for allocator_name, values in sorted(allocator_stats.items()):
    aggregates[allocator_name] = {
        "samples": len(values),
        "mean_seconds": sum(values) / len(values),
        "min_seconds": min(values),
        "max_seconds": max(values),
    }

recommended_allocator = None
if aggregates:
    recommended_allocator = min(
        aggregates.items(), key=lambda item: item[1]["mean_seconds"]
    )[0]

relative_deltas = {}
if "system" in aggregates and "jemalloc" in aggregates:
    system_mean = aggregates["system"]["mean_seconds"]
    jemalloc_mean = aggregates["jemalloc"]["mean_seconds"]
    if system_mean > 0:
        relative_deltas["jemalloc_vs_system_speedup_pct"] = (
            (system_mean - jemalloc_mean) / system_mean
        ) * 100.0
    if jemalloc_mean > 0:
        relative_deltas["system_vs_jemalloc_speedup_pct"] = (
            (jemalloc_mean - system_mean) / jemalloc_mean
        ) * 100.0

rss_pattern = re.compile(r"rss_(?P<allocator>[a-zA-Z0-9_-]+)_kib(?:_[^.]*)?\.txt$")
rss_samples = {}
for path in sorted(glob.glob(os.path.join(out_dir, "rss_*_kib*.txt"))):
    name = os.path.basename(path)
    match = rss_pattern.match(name)
    if match is None:
        continue
    allocator_name = match.group("allocator")
    text = pathlib.Path(path).read_text(encoding="utf-8")
    value_match = re.search(r"\d+", text)
    if value_match is None:
        continue
    rss_samples.setdefault(allocator_name, []).append(int(value_match.group(0)))

rss_kib_by_allocator = {}
for allocator_name, values in sorted(rss_samples.items()):
    rss_kib_by_allocator[allocator_name] = {
        "samples": len(values),
        "min_rss_kib": min(values),
        "max_rss_kib": max(values),
    }

if "system" in rss_kib_by_allocator and "jemalloc" in rss_kib_by_allocator:
    system_rss = rss_kib_by_allocator["system"]["max_rss_kib"]
    jemalloc_rss = rss_kib_by_allocator["jemalloc"]["max_rss_kib"]
    if system_rss > 0:
        relative_deltas["jemalloc_vs_system_max_rss_reduction_pct"] = (
            (system_rss - jemalloc_rss) / system_rss
        ) * 100.0

observed_fallback_reasons = []
events_file = pathlib.Path(events_path)
if events_file.exists():
    for line in events_file.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        reason = event.get("fallback_reason")
        if isinstance(reason, str) and reason and reason not in observed_fallback_reasons:
            observed_fallback_reasons.append(reason)

payload = {
    "schema": "pi.perf.allocator_strategy_summary.v1",
    "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "build_profile": build_profile,
    "allocator_requests": [
        token.strip() for token in allocators_csv.split(",") if token.strip()
    ],
    "allocator_fallback_policy": fallback_policy,
    "recommended_allocator": recommended_allocator,
    "allocator_stats": aggregates,
    "relative_deltas": relative_deltas,
    "rss_kib_by_allocator": rss_kib_by_allocator,
    "observed_fallback_reasons": observed_fallback_reasons,
    "hyperfine_matrix": matrix,
    "jsonl_path": jsonl_path,
    "events_path": events_path,
}

pathlib.Path(out_path).parent.mkdir(parents=True, exist_ok=True)
pathlib.Path(out_path).write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PYEOF
}

IFS=',' read -r -a TOOL_CALLS_SET <<< "$TOOL_CALLS_CSV"
IFS=',' read -r -a ALLOCATOR_SET <<< "$BENCH_ALLOCATORS_CSV"

LLVM_PROFDATA_BIN=""
if [[ "$BENCH_PGO_MODE" != "off" ]]; then
  LLVM_PROFDATA_BIN="$(find_llvm_profdata || true)"
fi

echo "cargo runner: ${CARGO_EXEC_PREFIX[*]}"
echo "pgo mode: $BENCH_PGO_MODE"

for ALLOCATOR_REQUEST in "${ALLOCATOR_SET[@]}"; do
  ALLOCATOR_REQUEST="${ALLOCATOR_REQUEST//[[:space:]]/}"
  ALLOCATOR_REQUEST="$(lower "$ALLOCATOR_REQUEST")"
  if [[ -z "$ALLOCATOR_REQUEST" ]]; then
    continue
  fi

  if [[ "$ALLOCATOR_REQUEST" != "system" && "$ALLOCATOR_REQUEST" != "jemalloc" && "$ALLOCATOR_REQUEST" != "auto" ]]; then
    echo "warning: unknown allocator request '$ALLOCATOR_REQUEST'; using 'auto'" >&2
    ALLOCATOR_REQUEST="auto"
  fi

  ALLOCATOR_BUILD_DIR="$OUT_DIR/build/$ALLOCATOR_REQUEST"
  mkdir -p "$ALLOCATOR_BUILD_DIR"
  BASELINE_BIN="$ALLOCATOR_BUILD_DIR/pijs_workload_baseline"
  PGO_BIN="$ALLOCATOR_BUILD_DIR/pijs_workload_pgo"
  BASELINE_BUILD_LOG="$ALLOCATOR_BUILD_DIR/build_baseline.log"
  PGO_BUILD_LOG="$ALLOCATOR_BUILD_DIR/build_pgo.log"
  PGO_TRAIN_LOG="$ALLOCATOR_BUILD_DIR/train.log"
  : > "$PGO_BUILD_LOG"
  : > "$PGO_TRAIN_LOG"
  {
    echo "allocator_request=$ALLOCATOR_REQUEST"
    echo "pgo_mode_requested=$BENCH_PGO_MODE"
    echo "build_profile=$BENCH_CARGO_PROFILE"
  } >>"$PGO_BUILD_LOG"
  PGO_PROFILE_STATE="not_requested"
  PGO_EFFECTIVE_MODE="off"
  PGO_FALLBACK_REASON=""
  EFFECTIVE_ALLOCATOR="system"
  ALLOCATOR_FALLBACK_REASON=""

  if ! build_variant_binary "$ALLOCATOR_REQUEST" "" "baseline" "$BASELINE_BUILD_LOG" "$BASELINE_BIN"; then
    echo "failed to build baseline binary for allocator '$ALLOCATOR_REQUEST' (see $BASELINE_BUILD_LOG)" >&2
    exit 1
  fi

  if [[ -n "$ALLOCATOR_FALLBACK_REASON" ]]; then
    PGO_FALLBACK_REASON="$ALLOCATOR_FALLBACK_REASON"
  fi

  if [[ "$BENCH_PGO_MODE" == "train" || "$BENCH_PGO_MODE" == "compare" ]]; then
    if [[ -z "$LLVM_PROFDATA_BIN" ]]; then
      PGO_PROFILE_STATE="missing_tool"
      PGO_FALLBACK_REASON="llvm_profdata_missing"
    else
      mkdir -p "$BENCH_PGO_PROFILE_DIR"
      rm -f "$BENCH_PGO_PROFILE_DIR"/*.profraw "$BENCH_PGO_PROFILE_DATA"
      if build_binary_for_allocator "$ALLOCATOR_REQUEST" "-Cprofile-generate=$BENCH_PGO_PROFILE_DIR" "$PGO_BUILD_LOG"; then
        if "$BIN" --iterations "$BENCH_PGO_TRAIN_ITERATIONS" --tool-calls "$BENCH_PGO_TRAIN_TOOL_CALLS" >"$PGO_TRAIN_LOG" 2>&1; then
          if compgen -G "$BENCH_PGO_PROFILE_DIR/*.profraw" >/dev/null; then
            if "$LLVM_PROFDATA_BIN" merge -o "$BENCH_PGO_PROFILE_DATA" "$BENCH_PGO_PROFILE_DIR"/*.profraw >>"$PGO_TRAIN_LOG" 2>&1; then
              PGO_PROFILE_STATE="generated"
            else
              PGO_PROFILE_STATE="corrupt"
              PGO_FALLBACK_REASON="profdata_merge_failed"
            fi
          else
            PGO_PROFILE_STATE="missing"
            PGO_FALLBACK_REASON="missing_profile_data"
          fi
        else
          PGO_PROFILE_STATE="missing"
          PGO_FALLBACK_REASON="pgo_training_run_failed"
        fi
      else
        PGO_PROFILE_STATE="missing"
        PGO_FALLBACK_REASON="pgo_instrumented_build_failed"
      fi
    fi
  fi

  if [[ "$BENCH_PGO_MODE" == "use" || "$BENCH_PGO_MODE" == "train" || "$BENCH_PGO_MODE" == "compare" ]]; then
    if [[ "$PGO_PROFILE_STATE" == "not_requested" ]]; then
      if [[ ! -f "$BENCH_PGO_PROFILE_DATA" ]]; then
        PGO_PROFILE_STATE="missing"
        PGO_FALLBACK_REASON="${PGO_FALLBACK_REASON:-missing_profile_data}"
      elif [[ ! -s "$BENCH_PGO_PROFILE_DATA" ]]; then
        PGO_PROFILE_STATE="corrupt"
        PGO_FALLBACK_REASON="${PGO_FALLBACK_REASON:-corrupt_profile_data}"
      elif [[ -n "$LLVM_PROFDATA_BIN" ]] && ! "$LLVM_PROFDATA_BIN" show "$BENCH_PGO_PROFILE_DATA" >/dev/null 2>&1; then
        PGO_PROFILE_STATE="corrupt"
        PGO_FALLBACK_REASON="${PGO_FALLBACK_REASON:-corrupt_profile_data}"
      else
        PGO_PROFILE_STATE="present"
      fi
    fi

    if [[ "$PGO_PROFILE_STATE" == "generated" || "$PGO_PROFILE_STATE" == "present" ]]; then
      if build_variant_binary "$ALLOCATOR_REQUEST" "-Cprofile-use=$BENCH_PGO_PROFILE_DATA -Cllvm-args=-pgo-warn-missing-function" "pgo" "$PGO_BUILD_LOG" "$PGO_BIN"; then
        PGO_EFFECTIVE_MODE="pgo"
      elif [[ "$BENCH_PGO_ALLOW_FALLBACK" == "1" ]]; then
        cp "$BASELINE_BIN" "$PGO_BIN"
        chmod +x "$PGO_BIN"
        PGO_EFFECTIVE_MODE="baseline_fallback"
        PGO_FALLBACK_REASON="${PGO_FALLBACK_REASON:-profile_use_build_failed}"
        echo "profile-use build failed; using baseline fallback (reason=$PGO_FALLBACK_REASON)" >>"$PGO_BUILD_LOG"
      else
        echo "PGO profile-use build failed and fallback disabled" >&2
        exit 1
      fi
    elif [[ "$BENCH_PGO_ALLOW_FALLBACK" == "1" ]]; then
      cp "$BASELINE_BIN" "$PGO_BIN"
      chmod +x "$PGO_BIN"
      PGO_EFFECTIVE_MODE="baseline_fallback"
      PGO_FALLBACK_REASON="${PGO_FALLBACK_REASON:-missing_profile_data}"
      echo "profile data unavailable (state=$PGO_PROFILE_STATE); using baseline fallback (reason=$PGO_FALLBACK_REASON)" >>"$PGO_BUILD_LOG"
    else
      echo "PGO profile data unavailable (state=$PGO_PROFILE_STATE) and fallback disabled" >&2
      exit 1
    fi
  fi

  emit_pgo_event "build" "$ALLOCATOR_REQUEST" "$EFFECTIVE_ALLOCATOR" "$PGO_EFFECTIVE_MODE" "$PGO_PROFILE_STATE" "$PGO_FALLBACK_REASON" "$PGO_BUILD_LOG" ""

  for TOOL_CALLS in "${TOOL_CALLS_SET[@]}"; do
    TOOL_CALLS="${TOOL_CALLS//[[:space:]]/}"
    if [[ -z "$TOOL_CALLS" ]]; then
      continue
    fi

    if [[ "$BENCH_PGO_MODE" == "compare" ]]; then
      HYPERFINE_BASE="$OUT_DIR/hyperfine_pijs_workload_${ITERATIONS}x${TOOL_CALLS}_${BENCH_CARGO_PROFILE}_${ALLOCATOR_REQUEST}_baseline_effective-${EFFECTIVE_ALLOCATOR}.json"
      HYPERFINE_PGO="$OUT_DIR/hyperfine_pijs_workload_${ITERATIONS}x${TOOL_CALLS}_${BENCH_CARGO_PROFILE}_${ALLOCATOR_REQUEST}_pgo-${PGO_EFFECTIVE_MODE}_effective-${EFFECTIVE_ALLOCATOR}.json"
      PGO_DELTA="$OUT_DIR/pgo_delta_${ITERATIONS}x${TOOL_CALLS}_${BENCH_CARGO_PROFILE}_${ALLOCATOR_REQUEST}_effective-${EFFECTIVE_ALLOCATOR}.json"

      CMD_BASE="PI_BENCH_BUILD_PROFILE=${BENCH_CARGO_PROFILE} PI_BENCH_ALLOCATOR=${ALLOCATOR_REQUEST} $BASELINE_BIN --iterations ${ITERATIONS} --tool-calls ${TOOL_CALLS}"
      CMD_PGO="PI_BENCH_BUILD_PROFILE=${BENCH_CARGO_PROFILE} PI_BENCH_ALLOCATOR=${ALLOCATOR_REQUEST} $PGO_BIN --iterations ${ITERATIONS} --tool-calls ${TOOL_CALLS}"

      hyperfine --warmup "$HYPERFINE_WARMUP" --runs "$HYPERFINE_RUNS" --export-json "$HYPERFINE_BASE" "$CMD_BASE"
      hyperfine --warmup "$HYPERFINE_WARMUP" --runs "$HYPERFINE_RUNS" --export-json "$HYPERFINE_PGO" "$CMD_PGO"

      write_pgo_delta_json "$HYPERFINE_BASE" "$HYPERFINE_PGO" "$PGO_DELTA" "$ALLOCATOR_REQUEST" "$EFFECTIVE_ALLOCATOR" "$TOOL_CALLS" "$PGO_EFFECTIVE_MODE" "$PGO_FALLBACK_REASON"
      emit_pgo_event "comparison" "$ALLOCATOR_REQUEST" "$EFFECTIVE_ALLOCATOR" "$PGO_EFFECTIVE_MODE" "$PGO_PROFILE_STATE" "$PGO_FALLBACK_REASON" "" "$PGO_DELTA"

      PI_BENCH_BUILD_PROFILE="$BENCH_CARGO_PROFILE" PI_BENCH_ALLOCATOR="$ALLOCATOR_REQUEST" "$PGO_BIN" --iterations "$ITERATIONS" --tool-calls "$TOOL_CALLS" >>"$JSONL_OUT"
    else
      HYPERFINE_OUT="$OUT_DIR/hyperfine_pijs_workload_${ITERATIONS}x${TOOL_CALLS}_${BENCH_CARGO_PROFILE}_${ALLOCATOR_REQUEST}_effective-${EFFECTIVE_ALLOCATOR}.json"
      # Keep release-gate evidence bound to Cargo's canonical
      # target/<profile>/examples path so the producer can independently
      # verify the executable profile. Copied PGO comparison variants remain
      # diagnostic-only and therefore fail the gate's path verification.
      ACTIVE_BIN="$BIN"
      if [[ "$PGO_EFFECTIVE_MODE" == "pgo" || "$PGO_EFFECTIVE_MODE" == "baseline_fallback" ]]; then
        ACTIVE_BIN="$PGO_BIN"
      fi
      CMD="PI_BENCH_BUILD_PROFILE=${BENCH_CARGO_PROFILE} PI_BENCH_ALLOCATOR=${ALLOCATOR_REQUEST} $ACTIVE_BIN --iterations ${ITERATIONS} --tool-calls ${TOOL_CALLS}"

      hyperfine --warmup "$HYPERFINE_WARMUP" --runs "$HYPERFINE_RUNS" --export-json "$HYPERFINE_OUT" "$CMD"
      PI_BENCH_BUILD_PROFILE="$BENCH_CARGO_PROFILE" PI_BENCH_ALLOCATOR="$ALLOCATOR_REQUEST" "$ACTIVE_BIN" --iterations "$ITERATIONS" --tool-calls "$TOOL_CALLS" >>"$JSONL_OUT"
    fi
  done

  if [[ -n "$ALLOCATOR_FALLBACK_REASON" ]]; then
    echo "allocator request '$ALLOCATOR_REQUEST' ran as '$EFFECTIVE_ALLOCATOR' ($ALLOCATOR_FALLBACK_REASON)"
  else
    echo "allocator request '$ALLOCATOR_REQUEST' ran as '$EFFECTIVE_ALLOCATOR'"
  fi

  if [[ "$PGO_EFFECTIVE_MODE" != "off" ]]; then
    if [[ -n "$PGO_FALLBACK_REASON" ]]; then
      echo "pgo mode '$BENCH_PGO_MODE' effective='$PGO_EFFECTIVE_MODE' profile_state='$PGO_PROFILE_STATE' fallback='$PGO_FALLBACK_REASON'"
    else
      echo "pgo mode '$BENCH_PGO_MODE' effective='$PGO_EFFECTIVE_MODE' profile_state='$PGO_PROFILE_STATE'"
    fi
  fi
done

emit_allocator_strategy_summary

echo "Wrote artifacts:"
echo "  - profile=$BENCH_CARGO_PROFILE"
echo "  - allocators=$BENCH_ALLOCATORS_CSV"
echo "  - pgo_mode=$BENCH_PGO_MODE"
echo "  - pgo_profile_data=$BENCH_PGO_PROFILE_DATA"
echo "  - $JSONL_OUT"
echo "  - $BENCH_PGO_EVENTS_JSONL"
echo "  - $BENCH_ALLOCATOR_SUMMARY_JSON"
echo "  - $OUT_DIR/hyperfine_pijs_workload_${ITERATIONS}x*_${BENCH_CARGO_PROFILE}_*_effective-*.json"
if [[ "$BENCH_PGO_MODE" == "compare" ]]; then
  echo "  - $OUT_DIR/pgo_delta_${ITERATIONS}x*_${BENCH_CARGO_PROFILE}_*_effective-*.json"
fi
