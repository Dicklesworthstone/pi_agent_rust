#!/usr/bin/env bash
# scripts/perf/orchestrate.sh — Reproducible benchmark/test orchestration with artifact bundles.
#
# One-command orchestrator that executes all benchmark and performance test suites
# in a deterministic environment, collects structured JSONL evidence, and produces
# a versioned artifact bundle with run manifest and integrity checksums.
#
# Bead: bd-3ar8v.1.8
# Depends on: bd-3ar8v.1.7 (structured logging contract), bd-3ar8v.1.1 (benchmark protocol)
#
# Usage:
#   ./scripts/perf/orchestrate.sh                           # full run (all suites)
#   ./scripts/perf/orchestrate.sh --profile quick            # PR-safe subset
#   ./scripts/perf/orchestrate.sh --profile ci               # CI-optimized run
#   ./scripts/perf/orchestrate.sh --suite bench_scenario     # single suite
#   ./scripts/perf/orchestrate.sh --suite perf_budgets       # budget checks only
#   ./scripts/perf/orchestrate.sh --list                     # list available suites
#   ./scripts/perf/orchestrate.sh --skip-build               # skip cargo build step
#   ./scripts/perf/orchestrate.sh --skip-env-check           # skip environment validation
#   ./scripts/perf/orchestrate.sh --output-dir <path>        # custom output directory
#   ./scripts/perf/orchestrate.sh --bundle                   # create tar.gz bundle at end
#   ./scripts/perf/orchestrate.sh --validate-only <dir>      # validate existing bundle
#   ./scripts/perf/orchestrate.sh --require-rch              # require remote offload
#   ./scripts/perf/orchestrate.sh --no-rch                   # force local cargo execution
#
# Environment:
#   CARGO_TARGET_DIR          Cargo target directory (default: target/)
#   PERF_OUTPUT_DIR           Override output directory (default: target/perf/runs/<timestamp>)
#   PERF_PROFILE              Build profile: release, perf, debug (default: perf)
#   PERF_PARALLELISM          Test parallelism (default: 1 for determinism)
#   PERF_BUILD_JOBS           Cargo/Nextest build parallelism (default: 8)
#   PERF_PGO_MODE             PGO mode: off, train, use, compare (default: off)
#   PERF_PGO_PROFILE_DATA     Explicit .profdata path for profile-use mode
#   PERF_PGO_ALLOW_FALLBACK   Fail-closed toggle when PGO data is missing/corrupt (default: 1)
#   PERF_CROSS_ENV_BASELINES  Semicolon-delimited label=path list for cross-env diagnosis
#                             (example: ci=tests/perf/reports/baseline_variance.json;canary=/tmp/baseline_canary.json)
#   PERF_CROSS_ENV_VARIANCE_ALERT_PCT
#                             Cross-env spread threshold percent (default: 10.0)
#   PERF_CROSS_ENV_ENFORCE    If 1, fail run when cross-env diagnosis emits alerts
#   PERF_REMOTE_TARGET_DIR    Optional remote CARGO_TARGET_DIR prefix recorded in artifact staging manifests
#   PERF_EVIDENCE_DIR         Optional repo-visible staged evidence root consumed by perf_budgets report generation
#   PERF_EVIDENCE_DIRS        Optional path-list of additional staged evidence roots
#   PERF_FAULT_INJECTION_ROOT Optional persistence-fault evidence root for hermetic runs
#   PERF_MAX_BENCH_ENV_NOISE_SCORE  Maximum admissible benches/bench_env.rs score (default: 0)
#   PERF_EVIDENCE_CACHE_DIR   Optional perf evidence cache directory (default: $CARGO_TARGET_DIR/perf/evidence_cache)
#   PI_PERF_EVIDENCE_CACHE_TTL_HOURS
#                             Maximum reusable perf evidence cache TTL in hours (default: 168)
#   PERF_QUICK                Set to 1 for PR-safe subset (same as --profile quick)
#   PERF_SKIP_CRITERION       Set to 1 to skip criterion benchmarks
#   PERF_SKIP_BUILD           Set to 1 to skip cargo build step
#   CI_CORRELATION_ID         Correlation ID for artifact tracing (auto-generated if unset)
#   BENCH_QUICK               Forwarded to perf_bench_harness (1 = fewer iterations)
#   BENCH_ITERATIONS          Override iteration count for bench harness
#   PERF_REGRESSION_FULL      Forwarded to perf_regression (1 = full mode)
#   PI_PERF_STRICT            Set to 1 to fail CI-enforced budgets on NO_DATA (auto-set for ci/full profiles)
#   PERF_CARGO_RUNNER         Cargo runner mode: rch | auto | local (default: rch)
#   RCH_REQUIRE_REMOTE        RCH proof mode: fail closed instead of falling back locally

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

# ─── Configuration ───────────────────────────────────────────────────────────

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
CARGO_PROFILE="${PERF_PROFILE:-perf}"
TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
OUTPUT_DIR="${PERF_OUTPUT_DIR:-$TARGET_DIR/perf/runs/$TIMESTAMP}"
PREFLIGHT_BEFORE_REFRESH_PATH="$OUTPUT_DIR/results/perf_budget_preflight_before_refresh.json"
PREFLIGHT_AFTER_RUN_PATH="$OUTPUT_DIR/results/perf_budget_preflight.json"
STAGING_MANIFEST_PATH="$OUTPUT_DIR/results/perf_artifact_staging_manifest.json"
BUILD_TESTS_JSONL_PATH="$OUTPUT_DIR/logs/build_tests.jsonl"
PERF_BUDGET_BINARY_ATTESTATION_PATH="$OUTPUT_DIR/results/perf_budgets_test_binary.json"
PARALLELISM="${PERF_PARALLELISM:-1}"
BUILD_JOBS="${PERF_BUILD_JOBS:-8}"
PGO_MODE="${PERF_PGO_MODE:-off}"
PGO_PROFILE_DATA="${PERF_PGO_PROFILE_DATA:-$TARGET_DIR/perf/$CARGO_PROFILE/pgo_profile/pijs_workload.profdata}"
PGO_ALLOW_FALLBACK="${PERF_PGO_ALLOW_FALLBACK:-1}"
CROSS_ENV_BASELINES="${PERF_CROSS_ENV_BASELINES:-}"
CROSS_ENV_VARIANCE_ALERT_PCT="${PERF_CROSS_ENV_VARIANCE_ALERT_PCT:-10.0}"
CROSS_ENV_ENFORCE="${PERF_CROSS_ENV_ENFORCE:-0}"
EVIDENCE_CACHE_DIR="${PERF_EVIDENCE_CACHE_DIR:-$TARGET_DIR/perf/evidence_cache}"
EVIDENCE_CACHE_TTL_HOURS="${PI_PERF_EVIDENCE_CACHE_TTL_HOURS:-168}"
CORRELATION_ID="${CI_CORRELATION_ID:-}"
PROFILE="full"
SKIP_BUILD="${PERF_SKIP_BUILD:-0}"
SKIP_ENV_CHECK=0
SKIP_CRITERION="${PERF_SKIP_CRITERION:-0}"
CREATE_BUNDLE=0
VALIDATE_ONLY=""
GIT_COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")"
GIT_COMMIT_FULL="$(git rev-parse HEAD 2>/dev/null || echo "unknown")"
GIT_STATUS_AVAILABLE=false
GIT_STATUS_PORCELAIN=""
GIT_DIRTY=true
if GIT_STATUS_PORCELAIN="$(git status --porcelain=v1 --untracked-files=all 2>/dev/null)"; then
  GIT_STATUS_AVAILABLE=true
  if [[ -z "$GIT_STATUS_PORCELAIN" ]]; then
    GIT_DIRTY=false
  fi
fi
CARGO_RUNNER_REQUEST="${PERF_CARGO_RUNNER:-rch}" # rch | auto | local
CARGO_RUNNER_MODE="local"
declare -a CARGO_RUNNER_ARGS=("cargo")
declare -a PERF_BENCH_RUNNER_ARGS=("cargo")
SEEN_NO_RCH=false
SEEN_REQUIRE_RCH=false
ARTIFACT_STAGING_STATUS="not_generated"
ARTIFACT_STAGING_MISSING_REQUIRED=0
ARTIFACT_STAGING_STALE_REQUIRED=0
ARTIFACT_STAGING_BLOCKERS=0

# Suite registry: name -> cargo test target or bench name
declare -A SUITE_TARGETS=(
  [bench_schema]="bench_schema"
  [bench_scenario]="bench_scenario_runner"
  [ext_bench_harness]="ext_bench_harness"
  [perf_bench_harness]="perf_bench_harness"
  [perf_budgets]="perf_budgets"
  [perf_regression]="perf_regression"
  [perf_comparison]="perf_comparison"
  [perf_baseline_variance]="perf_baseline_variance"
)

declare -A CRITERION_BENCHES=(
  [criterion_tools]="tools"
  [criterion_extensions]="extensions"
  [criterion_system]="system"
  [criterion_semantic_context]="semantic_context"
)

SELECTED_SUITES=()
LIST_ONLY=false

# ─── Helpers ─────────────────────────────────────────────────────────────────

red()    { printf '\033[0;31m%s\033[0m\n' "$*"; }
green()  { printf '\033[0;32m%s\033[0m\n' "$*"; }
yellow() { printf '\033[0;33m%s\033[0m\n' "$*"; }
bold()   { printf '\033[1m%s\033[0m\n' "$*"; }
dim()    { printf '\033[2m%s\033[0m\n' "$*"; }

die() { red "ERROR: $*" >&2; exit 1; }

require_positive_integer() {
  local name="$1"
  local value="$2"
  if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
    die "$name must be a positive integer, got: $value"
  fi
}

log_phase() {
  echo ""
  bold "═══ $1 ═══"
  echo ""
}

log_step() {
  echo "  → $1"
}

log_ok() {
  green "  ✓ $1"
}

log_warn() {
  yellow "  ⚠ $1"
}

log_fail() {
  red "  ✗ $1"
}

epoch_ms() {
  # Milliseconds since epoch (portable)
  python3 -c "import time; print(int(time.time() * 1000))" 2>/dev/null \
    || date +%s%3N 2>/dev/null \
    || echo "0"
}

sha256_file() {
  sha256sum "$1" 2>/dev/null | cut -d' ' -f1
}

generate_correlation_id() {
  python3 -c "import uuid; print(uuid.uuid4().hex)" 2>/dev/null \
    || head -c 16 /dev/urandom | xxd -p 2>/dev/null \
    || echo "local-$(date +%s)-$$"
}

run_budget_preflight() {
  local output_path="$1"
  shift
  local args=(
    --repo-root "$PROJECT_ROOT"
    --cargo-target-dir "$TARGET_DIR"
    --evidence-cache-dir "$EVIDENCE_CACHE_DIR"
    --cache-ttl-hours "$EVIDENCE_CACHE_TTL_HOURS"
    --cache-profile "$CARGO_PROFILE"
    --cache-git-commit "$GIT_COMMIT_FULL"
    --expected-correlation-id "$CORRELATION_ID"
    --skip-rch-check
  )
  PERF_EVIDENCE_DIR="$OUTPUT_DIR/results" \
    python3 "$SCRIPT_DIR/preflight_budget_inputs.py" "${args[@]}" "$@" > "$output_path"
}

run_artifact_staging_manifest() {
  local output_path="$1"
  shift
  local args=(
    --repo-root "$PROJECT_ROOT"
    --cargo-target-dir "$TARGET_DIR"
    --local-results-dir "$OUTPUT_DIR/results"
    --runner-mode "$CARGO_RUNNER_MODE"
    --evidence-cache-dir "$EVIDENCE_CACHE_DIR"
    --cache-ttl-hours "$EVIDENCE_CACHE_TTL_HOURS"
    --cache-profile "$CARGO_PROFILE"
    --cache-git-commit "$GIT_COMMIT_FULL"
    --run-id "$CORRELATION_ID"
    --expected-correlation-id "$CORRELATION_ID"
    --update-evidence-cache
    --output "$output_path"
  )
  if [[ -n "${PERF_REMOTE_TARGET_DIR:-}" ]]; then
    args+=(--remote-target-dir "$PERF_REMOTE_TARGET_DIR")
  fi
  PERF_EVIDENCE_DIR="$OUTPUT_DIR/results" \
    python3 "$SCRIPT_DIR/artifact_staging.py" "${args[@]}" "$@"
}

# ─── CLI Parsing ─────────────────────────────────────────────────────────────

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile)
      PROFILE="$2"
      shift 2
      ;;
    --suite)
      SELECTED_SUITES+=("$2")
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD=1
      shift
      ;;
    --skip-env-check)
      SKIP_ENV_CHECK=1
      shift
      ;;
    --bundle)
      CREATE_BUNDLE=1
      shift
      ;;
    --validate-only)
      VALIDATE_ONLY="$2"
      shift 2
      ;;
    --no-rch)
      if [[ "$SEEN_REQUIRE_RCH" == true ]]; then
        die "Cannot combine --no-rch and --require-rch"
      fi
      SEEN_NO_RCH=true
      CARGO_RUNNER_REQUEST="local"
      shift
      ;;
    --require-rch)
      if [[ "$SEEN_NO_RCH" == true ]]; then
        die "Cannot combine --require-rch and --no-rch"
      fi
      SEEN_REQUIRE_RCH=true
      CARGO_RUNNER_REQUEST="rch"
      shift
      ;;
    --list)
      LIST_ONLY=true
      shift
      ;;
    --help|-h)
      sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
      exit 0
      ;;
    *)
      die "Unknown flag: $1 (try --help)"
      ;;
  esac
done

# Quick profile shorthand
if [[ "${PERF_QUICK:-0}" == "1" ]]; then
  PROFILE="quick"
fi

RCH_PROOF_REQUIRED=false
case "${RCH_REQUIRE_REMOTE:-0}" in
  1|true|TRUE|yes|YES|on|ON) RCH_PROOF_REQUIRED=true ;;
esac
if [[ "$SEEN_REQUIRE_RCH" == true ]]; then
  RCH_PROOF_REQUIRED=true
fi
if [[ "$SEEN_NO_RCH" == true && "$RCH_PROOF_REQUIRED" == true ]]; then
  die "Cannot combine --no-rch with RCH_REQUIRE_REMOTE proof mode"
fi

# ─── List mode ───────────────────────────────────────────────────────────────

if [[ "$LIST_ONLY" == "true" ]]; then
  bold "Available performance suites:"
  echo ""
  echo "  Test suites:"
  for suite in "${!SUITE_TARGETS[@]}"; do
    printf "    %-25s cargo test --test %s\n" "$suite" "${SUITE_TARGETS[$suite]}"
  done | sort
  echo ""
  echo "  Criterion benchmarks:"
  for bench in "${!CRITERION_BENCHES[@]}"; do
    printf "    %-25s cargo bench --bench %s\n" "$bench" "${CRITERION_BENCHES[$bench]}"
  done | sort
  echo ""
  echo "  Profiles: full, quick, ci"
  exit 0
fi

# ─── Validate-only mode ─────────────────────────────────────────────────────

if [[ -n "$VALIDATE_ONLY" ]]; then
  log_phase "Validating existing bundle: $VALIDATE_ONLY"

  errors=0

  if [[ ! -f "$VALIDATE_ONLY/manifest.json" ]]; then
    log_fail "Missing manifest.json"
    errors=$((errors + 1))
  else
    log_ok "manifest.json present"
  fi

  if [[ ! -f "$VALIDATE_ONLY/checksums.sha256" ]]; then
    log_fail "Missing checksums.sha256"
    errors=$((errors + 1))
  else
    log_ok "checksums.sha256 present"
    pushd "$VALIDATE_ONLY" >/dev/null
    if sha256sum -c checksums.sha256 --quiet 2>/dev/null; then
      log_ok "All checksums verified"
    else
      log_fail "Checksum verification failed"
      errors=$((errors + 1))
    fi
    popd >/dev/null
  fi

  if [[ ! -d "$VALIDATE_ONLY/results" ]]; then
    log_fail "Missing results/ directory"
    errors=$((errors + 1))
  else
    result_count=$(find "$VALIDATE_ONLY/results" -name "*.json" -o -name "*.jsonl" 2>/dev/null | wc -l)
    log_ok "results/ directory present ($result_count artifact files)"
  fi

  if [[ "$errors" -gt 0 ]]; then
    die "Validation failed with $errors error(s)"
  fi
  green "Bundle validation passed."
  exit 0
fi

require_positive_integer "PERF_PARALLELISM" "$PARALLELISM"
require_positive_integer "PERF_BUILD_JOBS" "$BUILD_JOBS"

# ─── Cargo Runner Resolution ────────────────────────────────────────────────

if [[ "$CARGO_RUNNER_REQUEST" != "rch" && "$CARGO_RUNNER_REQUEST" != "auto" && "$CARGO_RUNNER_REQUEST" != "local" ]]; then
  die "Invalid PERF_CARGO_RUNNER value: $CARGO_RUNNER_REQUEST (expected: rch|auto|local)"
fi

if [[ "$CARGO_RUNNER_REQUEST" == "rch" ]]; then
  if ! command -v rch >/dev/null 2>&1; then
    die "PERF_CARGO_RUNNER=rch requested, but 'rch' is not available in PATH."
  fi
  if [[ "$RCH_PROOF_REQUIRED" == true ]]; then
    export RCH_REQUIRE_REMOTE=1
  fi
  if ! rch check --quiet >/dev/null 2>&1; then
    if [[ "${RCH_REQUIRE_REMOTE:-0}" == "1" ]]; then
      log_warn "'rch check' reports fleet degradation; proceeding with fail-closed remote execution."
    else
      die "'rch check' failed; refusing heavy local cargo fallback. Fix rch or pass --no-rch."
    fi
  fi
  CARGO_RUNNER_MODE="rch"
  CARGO_RUNNER_ARGS=("rch" "exec" "--" "cargo")
elif [[ "$CARGO_RUNNER_REQUEST" == "auto" ]] && command -v rch >/dev/null 2>&1; then
  if rch check --quiet >/dev/null 2>&1; then
    CARGO_RUNNER_MODE="rch"
    CARGO_RUNNER_ARGS=("rch" "exec" "--" "cargo")
  else
    log_warn "rch detected but unhealthy; auto mode will run cargo locally (set --require-rch to fail fast)."
  fi
fi

if [[ "$CARGO_RUNNER_MODE" == "rch" ]]; then
  for required_env in \
    BENCH_OUTPUT_DIR \
    BENCH_OUTPUT_TARGET_SUBDIR \
    BENCH_QUICK \
    BENCH_ITERATIONS \
    PI_BENCH_RUN_ID \
    CARGO_BUILD_JOBS \
    PERF_REGRESSION_OUTPUT \
    PERF_RELEASE_BINARY_PATH \
    CI_CORRELATION_ID \
    VERGEN_GIT_SHA \
    VERGEN_GIT_DIRTY \
    RUST_TEST_THREADS \
    PI_IDLE_RSS_RAW_RELATIVE_PATH \
    PI_IDLE_RSS_SOURCE_COMMIT \
    PI_IDLE_RSS_SOURCE_DIRTY \
    PI_IDLE_RSS_CORRELATION_ID \
    PI_BENCH_BUILD_PROFILE \
    PI_PERF_STRICT; do
    case ",${RCH_ENV_ALLOWLIST:-}," in
      *",$required_env,"*) ;;
      *) RCH_ENV_ALLOWLIST="${RCH_ENV_ALLOWLIST:+$RCH_ENV_ALLOWLIST,}$required_env" ;;
    esac
  done
  export RCH_ENV_ALLOWLIST
fi

# ─── Profile-based suite selection ───────────────────────────────────────────

resolve_suites() {
  case "$PROFILE" in
    full)
      # All test suites + criterion benchmarks
      SELECTED_SUITES=("${!SUITE_TARGETS[@]}")
      if [[ "$SKIP_CRITERION" != "1" ]]; then
        SELECTED_SUITES+=("${!CRITERION_BENCHES[@]}")
      fi
      ;;
    quick)
      # Fast subset: schema validation + budgets only, no criterion
      SELECTED_SUITES=(bench_schema perf_budgets)
      SKIP_CRITERION=1
      ;;
    ci)
      # CI: all test suites, skip heavy criterion benches
      SELECTED_SUITES=("${!SUITE_TARGETS[@]}")
      SKIP_CRITERION=1
      ;;
    *)
      die "Unknown profile: $PROFILE (available: full, quick, ci)"
      ;;
  esac
}

apply_profile_settings() {
  case "$PROFILE" in
    full|ci) export PI_PERF_STRICT=1 ;;
    quick)
      SKIP_CRITERION=1
      export BENCH_QUICK=1
      ;;
    *) die "Unknown profile: $PROFILE (available: full, quick, ci)" ;;
  esac
}

apply_profile_settings

if [[ ${#SELECTED_SUITES[@]} -eq 0 ]]; then
  resolve_suites
fi

suite_selected() {
  local wanted="$1"
  for suite in "${SELECTED_SUITES[@]}"; do
    if [[ "$suite" == "$wanted" ]]; then
      return 0
    fi
  done
  return 1
}

verify_current_clean_source_identity() {
  local label="$1"
  local observed_commit observed_status
  if ! observed_commit="$(git rev-parse HEAD 2>/dev/null)"; then
    log_fail "$label: Git commit identity is unavailable"
    return 1
  fi
  if [[ ! "$observed_commit" =~ ^[0-9a-f]{40}$ ]]; then
    log_fail "$label: Git commit identity is not a full SHA-1: $observed_commit"
    return 1
  fi
  if [[ "$observed_commit" != "$GIT_COMMIT_FULL" ]]; then
    log_fail "$label: Git HEAD drifted from $GIT_COMMIT_FULL to $observed_commit"
    return 1
  fi
  if ! observed_status="$(git status --porcelain=v1 --untracked-files=all 2>/dev/null)"; then
    log_fail "$label: Git status is unavailable"
    return 1
  fi
  if [[ -n "$observed_status" ]]; then
    log_fail "$label: source tree is dirty"
    return 1
  fi
  return 0
}

if [[ "$CARGO_RUNNER_MODE" == "rch" ]] \
  && suite_selected "perf_bench_harness"; then
  if [[ ! "$GIT_COMMIT_FULL" =~ ^[0-9a-f]{40}$ ]]; then
    die "RCH performance proof requires a full Git commit identity, got: $GIT_COMMIT_FULL"
  fi
  if [[ "$GIT_STATUS_AVAILABLE" != true ]]; then
    die "RCH performance proof requires an available Git status"
  fi
  if [[ "$GIT_DIRTY" != false ]]; then
    die "RCH performance proof requires a clean source tree"
  fi
  if ! verify_current_clean_source_identity "RCH performance proof admission"; then
    die "RCH performance proof source identity is not stable"
  fi
  # The accepted extension benchmark always uses a clean committed-source pin.
  # The runner's receipt and the retrieved record's exact run/commit identity
  # jointly distinguish real remote evidence from fallback or stale output.
  PERF_BENCH_RUNNER_ARGS=(
    "rch" "exec"
    "--no-color"
    "--base" "$GIT_COMMIT_FULL"
    "--clean-overlay"
    "--no-overlay"
    "--" "cargo"
  )
else
  PERF_BENCH_RUNNER_ARGS=("${CARGO_RUNNER_ARGS[@]}")
fi

write_binary_size_measurement_control() {
  local binary_path="$1"
  local control_path="$TARGET_DIR/perf/release_evidence/binary_size_measurement.json"
  mkdir -p "$(dirname "$control_path")"

  python3 - \
    "$PROJECT_ROOT/Cargo.toml" \
    "$binary_path" \
    "$control_path" \
    "$GIT_COMMIT_FULL" \
    "$GIT_DIRTY" \
    "$CORRELATION_ID" <<'PY'
import hashlib
import json
import os
import sys
import tomllib
from datetime import datetime, timezone
from pathlib import Path

manifest_path = Path(sys.argv[1])
binary_path = Path(sys.argv[2]).resolve(strict=True)
control_path = Path(sys.argv[3])
source_commit = sys.argv[4]
source_dirty = sys.argv[5] == "true"
correlation_id = sys.argv[6]

with manifest_path.open("rb") as handle:
    manifest = tomllib.load(handle)
release = manifest.get("profile", {}).get("release", {})
opt_level = release.get("opt-level")
strip = release.get("strip")
if opt_level != "z" or strip is not True:
    raise SystemExit(
        "release binary measurement control requires Cargo.toml "
        "[profile.release] opt-level='z' and strip=true"
    )

digest = hashlib.sha256()
with binary_path.open("rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        digest.update(chunk)

payload = {
    "schema": "pi.perf.binary_size_measurement.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
    "run_id": correlation_id,
    "correlation_id": correlation_id,
    "source_commit": source_commit,
    "source_dirty": source_dirty,
    "binary_path": str(binary_path),
    "binary_sha256": digest.hexdigest(),
    "size_bytes": binary_path.stat().st_size,
    "cargo_profile": "release",
    "compiled_profile_family": "release",
    "compiled_opt_level": "z",
    "strip": True,
    "profile_source": "Cargo.toml#profile.release",
    "build_command": "cargo build --bin pi --release",
}
encoded = json.dumps(payload, indent=2, sort_keys=True) + "\n"
temporary_path = control_path.with_name(control_path.name + ".tmp")
temporary_path.write_text(encoded, encoding="utf-8")
os.replace(temporary_path, control_path)
PY
  log_ok "Binary-size measurement control: $control_path"
}

write_cold_load_measurement_control() {
  local result_dir="$1"
  local benchmark_exit_code="$2"
  local control_path="$TARGET_DIR/perf/release_evidence/cold_load_measurement.json"
  mkdir -p "$(dirname "$control_path")"

  python3 - \
    "$result_dir/stderr.log" \
    "$TARGET_DIR/criterion" \
    "$control_path" \
    "$benchmark_exit_code" \
    "$GIT_COMMIT_FULL" \
    "$GIT_DIRTY" \
    "$CORRELATION_ID" \
    "${PERF_MAX_BENCH_ENV_NOISE_SCORE:-0}" <<'PY'
import hashlib
import json
import os
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

stderr_path = Path(sys.argv[1])
criterion_root = Path(sys.argv[2])
control_path = Path(sys.argv[3])
benchmark_exit_code = int(sys.argv[4])
source_commit = sys.argv[5]
source_dirty = sys.argv[6] == "true"
correlation_id = sys.argv[7]
max_noise_score = int(sys.argv[8])
if not 0 <= max_noise_score <= 7:
    raise SystemExit("PERF_MAX_BENCH_ENV_NOISE_SCORE must be an integer in 0..7")

banner_pattern = re.compile(
    r'^\[bench-env\] os=(?P<os>.*?) arch=(?P<arch>\S+) cpu="(?P<cpu>.*?)" '
    r'cores=(?P<cores>\d+) mem_mb=(?P<mem_mb>\d+) governor=(?P<governor>\S+) '
    r'turbo=(?P<turbo>\S+) aslr=(?P<aslr>\S+) thp=(?P<thp>\S+) '
    r'noise_score=(?P<noise_score>\d+) config_hash=(?P<config_hash>[0-9a-f]{64})$'
)
banner_match = None
if stderr_path.is_file():
    for line in stderr_path.read_text(encoding="utf-8", errors="replace").splitlines():
        candidate = banner_pattern.fullmatch(line.strip())
        if candidate is not None:
            banner_match = candidate

payload = {
    "schema": "pi.perf.cold_load_measurement.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
    "run_id": correlation_id,
    "correlation_id": correlation_id,
    "source_commit": source_commit,
    "source_dirty": source_dirty,
    "benchmark_exit_code": benchmark_exit_code,
    "max_noise_score": max_noise_score,
    "bench_env_source": "benches/bench_env.rs",
    "status": "no_data",
    "reason": "criterion_extensions did not emit a parseable benches/bench_env.rs fingerprint",
    "bench_env": None,
    "measurements": {},
}

if banner_match is not None:
    values = banner_match.groupdict()
    bench_env = {
        "os": values["os"],
        "arch": values["arch"],
        "cpu_brand": values["cpu"],
        "cpu_cores": int(values["cores"]),
        "mem_total_mb": int(values["mem_mb"]),
        "governor": values["governor"],
        "turbo_boost": values["turbo"],
        "aslr": values["aslr"],
        "thp": values["thp"],
        "noise_score": int(values["noise_score"]),
        "config_hash": values["config_hash"],
    }
    bench_env_bytes = json.dumps(
        bench_env, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")
    measurements = {}
    for extension in ("hello", "pirate"):
        estimate_path = (
            criterion_root
            / "ext_load_init"
            / "load_init_cold"
            / extension
            / "new"
            / "estimates.json"
        )
        if not estimate_path.is_file():
            continue
        estimate_bytes = estimate_path.read_bytes()
        measurements[extension] = {
            "artifact_path": str(estimate_path.resolve(strict=True)),
            "artifact_sha256": hashlib.sha256(estimate_bytes).hexdigest(),
            "artifact_size_bytes": len(estimate_bytes),
        }
    if bench_env["noise_score"] > max_noise_score:
        payload.update(
            {
                "status": "no_data",
                "reason": f"bench_env noise score {bench_env['noise_score']} exceeds max_noise_score {max_noise_score}",
                "bench_env": bench_env,
                "bench_env_sha256": hashlib.sha256(bench_env_bytes).hexdigest(),
                "measurements": measurements,
            }
        )
    else:
        payload.update(
            {
                "status": "verified"
                if benchmark_exit_code == 0 and len(measurements) == 2
                else "no_data",
                "reason": None
                if benchmark_exit_code == 0 and len(measurements) == 2
                else "criterion_extensions failed or did not produce both cold-load estimates",
                "bench_env": bench_env,
                "bench_env_sha256": hashlib.sha256(bench_env_bytes).hexdigest(),
                "measurements": measurements,
            }
        )

encoded = json.dumps(payload, indent=2, sort_keys=True) + "\n"
temporary_path = control_path.with_name(control_path.name + ".tmp")
temporary_path.write_text(encoded, encoding="utf-8")
os.replace(temporary_path, control_path)
PY
  log_ok "Cold-load measurement control: $control_path"
}

write_idle_rss_measurement_control() {
  local producer_log="$1"
  local binary_path="$2"
  local control_path="$TARGET_DIR/perf/release_evidence/idle_memory_rss.json"
  mkdir -p "$(dirname "$control_path")"

  python3 - \
    "$PROJECT_ROOT/Cargo.toml" \
    "$producer_log" \
    "$binary_path" \
    "$control_path" \
    "$GIT_COMMIT_FULL" \
    "$GIT_DIRTY" \
    "$CORRELATION_ID" <<'PY'
import hashlib
import json
import os
import sys
import tomllib
from pathlib import Path

manifest_path = Path(sys.argv[1])
producer_log = Path(sys.argv[2])
binary_path = Path(sys.argv[3]).resolve(strict=True)
control_path = Path(sys.argv[4])
source_commit = sys.argv[5]
source_dirty = sys.argv[6] == "true"
correlation_id = sys.argv[7]

with manifest_path.open("rb") as handle:
    release = tomllib.load(handle).get("profile", {}).get("release", {})
if release.get("opt-level") != "z" or release.get("strip") is not True:
    raise SystemExit(
        "idle RSS measurement requires Cargo.toml [profile.release] "
        "opt-level='z' and strip=true"
    )
if source_dirty:
    raise SystemExit("idle RSS release evidence requires source_dirty=false")
if not producer_log.is_file():
    raise SystemExit(f"idle RSS producer log is missing: {producer_log}")
record_prefix = "[idle-rss-control] "
records = [
    line.split(record_prefix, 1)[1]
    for line in producer_log.read_text(encoding="utf-8", errors="replace").splitlines()
    if record_prefix in line
]
if len(records) != 1:
    raise SystemExit(
        f"idle RSS producer log must contain exactly one transport record, found {len(records)}"
    )
raw = json.loads(records[0])
if raw.get("schema") != "pi.perf.idle_rss_measurement.v1":
    raise SystemExit("idle RSS raw artifact has the wrong schema")
for field, expected in (
    ("run_id", correlation_id),
    ("correlation_id", correlation_id),
    ("source_commit", source_commit),
    ("source_dirty", False),
    ("process_name", "pi"),
    ("idle_state", "startup_before_user_input"),
    ("cargo_profile", "release"),
    ("build_command", "cargo build --bin pi --release"),
    ("bench_env_source", "benches/bench_env.rs"),
):
    if raw.get(field) != expected:
        raise SystemExit(
            f"idle RSS raw artifact field {field!r} does not match {expected!r}"
        )
if raw.get("allocator") not in {"system", "jemalloc"}:
    raise SystemExit("idle RSS raw artifact has an unknown allocator")

samples = raw.get("samples")
sample_count = raw.get("sample_count")
if not isinstance(samples, list) or not isinstance(sample_count, int):
    raise SystemExit("idle RSS raw artifact samples are malformed")
if sample_count < 5 or sample_count != len(samples):
    raise SystemExit("idle RSS raw artifact requires at least five declared samples")
pids = set()
rss_values = []
for sample in samples:
    if not isinstance(sample, dict):
        raise SystemExit("idle RSS sample must be an object")
    pid = sample.get("pid")
    rss_bytes = sample.get("rss_bytes")
    if (
        not isinstance(pid, int)
        or pid <= 0
        or pid in pids
        or sample.get("process_name") != "pi"
        or not isinstance(rss_bytes, int)
        or rss_bytes <= 0
    ):
        raise SystemExit("idle RSS samples require unique positive pi PIDs and RSS bytes")
    pids.add(pid)
    rss_values.append(rss_bytes)
max_rss = max(rss_values)
min_rss = min(rss_values)
if raw.get("rss_bytes") != max_rss:
    raise SystemExit("idle RSS aggregate must equal the maximum sample")
if raw.get("rss_spread_bytes") != max_rss - min_rss:
    raise SystemExit("idle RSS spread must equal maximum minus minimum")
if not any(
    sample["pid"] == raw.get("pid") and sample["rss_bytes"] == max_rss
    for sample in samples
):
    raise SystemExit("idle RSS representative PID must identify a maximum sample")
settle_ms = raw.get("settle_ms")
if not isinstance(settle_ms, int) or not 100 <= settle_ms <= 10_000:
    raise SystemExit("idle RSS settle_ms must be in 100..=10000")

bench_env = raw.get("bench_env")
bench_env_keys = [
    "os",
    "arch",
    "cpu_brand",
    "cpu_cores",
    "mem_total_mb",
    "governor",
    "turbo_boost",
    "aslr",
    "thp",
    "noise_score",
    "config_hash",
]
if not isinstance(bench_env, dict) or set(bench_env) != set(bench_env_keys):
    raise SystemExit("idle RSS benchmark environment fields are malformed")
bench_env_bytes = json.dumps(
    bench_env, separators=(",", ":"), ensure_ascii=False, sort_keys=True
).encode("utf-8")
observed_bench_env_sha256 = hashlib.sha256(bench_env_bytes).hexdigest()
if raw.get("bench_env_sha256") != observed_bench_env_sha256:
    raise SystemExit("idle RSS bench_env_sha256 does not match bench_env")

binary_digest = hashlib.sha256()
with binary_path.open("rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        binary_digest.update(chunk)
binary_sha256 = binary_digest.hexdigest()
if raw.get("binary_sha256") != binary_sha256:
    raise SystemExit("idle RSS remote binary hash does not match the retrieved release pi")

payload = dict(raw)
payload["binary_path"] = str(binary_path)
payload["binary_sha256"] = binary_sha256
encoded = json.dumps(payload, indent=2, sort_keys=True) + "\n"
temporary_path = control_path.with_name(control_path.name + ".tmp")
temporary_path.write_text(encoded, encoding="utf-8")
os.replace(temporary_path, control_path)
PY
  log_ok "Idle-RSS measurement control: $control_path"
}

# ─── Generate correlation ID ────────────────────────────────────────────────

if [[ -z "$CORRELATION_ID" ]]; then
  CORRELATION_ID="$(generate_correlation_id)"
fi

# ─── Setup output directory ─────────────────────────────────────────────────

mkdir -p "$OUTPUT_DIR/results"
mkdir -p "$OUTPUT_DIR/logs"

log_phase "Perf Orchestrator v1.0 (bd-3ar8v.1.8)"
log_step "Profile:        $PROFILE"
log_step "Output:         $OUTPUT_DIR"
log_step "Correlation ID: $CORRELATION_ID"
log_step "Git commit:     $GIT_COMMIT (dirty=$GIT_DIRTY)"
log_step "Cargo profile:  $CARGO_PROFILE"
log_step "Test threads:   $PARALLELISM"
log_step "Build jobs:     $BUILD_JOBS"
log_step "Evidence cache: $EVIDENCE_CACHE_DIR (ttl=${EVIDENCE_CACHE_TTL_HOURS}h)"
log_step "PGO mode:       $PGO_MODE"
log_step "PGO profile:    $PGO_PROFILE_DATA"
log_step "Cargo runner:   $CARGO_RUNNER_MODE (request=$CARGO_RUNNER_REQUEST)"
log_step "Timestamp:      $TIMESTAMP"
log_step "Suites:         ${SELECTED_SUITES[*]}"

# ─── Phase 1: Environment validation ────────────────────────────────────────

if [[ "$SKIP_ENV_CHECK" -eq 0 ]]; then
  log_phase "Phase 1: Environment Validation"

  env_warnings=0

  # Check disk space (need at least 1GB free)
  free_mb=$(df -m "$PROJECT_ROOT" 2>/dev/null | awk 'NR==2 {print $4}' || echo "0")
  if [[ "$free_mb" -lt 1024 ]]; then
    log_warn "Low disk space: ${free_mb}MB free (recommended: 1024MB+)"
    env_warnings=$((env_warnings + 1))
  else
    log_ok "Disk space: ${free_mb}MB free"
  fi

  # Check cargo/rustc
  if command -v cargo >/dev/null 2>&1; then
    rust_version="$(rustc --version 2>/dev/null || echo "unknown")"
    log_ok "Rust toolchain: $rust_version"
  else
    die "cargo/rustc not found in PATH"
  fi

  # Write cgroup-aware environment fingerprint.
  python3 "$SCRIPT_DIR/preflight_budget_inputs.py" \
    --host-fingerprint \
    --fingerprint-timestamp "$TIMESTAMP" \
    --fingerprint-build-profile "$CARGO_PROFILE" \
    --fingerprint-pgo-mode "$PGO_MODE" \
    --fingerprint-pgo-profile-data "$PGO_PROFILE_DATA" \
    --fingerprint-pgo-allow-fallback "$PGO_ALLOW_FALLBACK" \
    --fingerprint-git-commit "$GIT_COMMIT_FULL" \
    --fingerprint-git-dirty "$GIT_DIRTY" \
    --fingerprint-rust-version "$rust_version" \
    --fingerprint-cargo-runner-mode "$CARGO_RUNNER_MODE" \
    --fingerprint-cargo-runner-request "$CARGO_RUNNER_REQUEST" \
    --fingerprint-correlation-id "$CORRELATION_ID" \
    > "$OUTPUT_DIR/env_fingerprint.json"
  fingerprint_summary="$(
    python3 - "$OUTPUT_DIR/env_fingerprint.json" <<'PY'
import json
import sys

payload = json.loads(open(sys.argv[1], encoding="utf-8").read())
host = payload.get("host_fingerprint", {})
cgroup = host.get("cgroup", {})
numa = host.get("numa", {})
budget = payload.get("budget_profile", {})
print(
    "|".join(
        str(value)
        for value in (
            payload.get("cpu_model", "unknown"),
            payload.get("cpu_cores", "unknown"),
            payload.get("host_cpu_cores", "unknown"),
            payload.get("mem_total_mb", "unknown"),
            payload.get("host_mem_total_mb", "unknown"),
            payload.get("build_profile", "unknown"),
            cgroup.get("cpu_quota_cores"),
            cgroup.get("cpuset_cpu_count"),
            cgroup.get("memory_limit_mb"),
            numa.get("node_count"),
            budget.get("source", "unknown"),
            ",".join(payload.get("caveats", [])),
        )
    )
)
PY
  )"
  IFS='|' read -r \
    cpu_model \
    cpu_cores \
    host_cpu_cores \
    mem_total_mb \
    host_mem_total_mb \
    build_profile \
    cpu_quota_cores \
    cpuset_cpu_count \
    memory_limit_mb \
    numa_node_count \
    budget_profile_source \
    fingerprint_caveats <<< "$fingerprint_summary"
  log_ok "CPU: $cpu_model (target=$cpu_cores, host=$host_cpu_cores, quota=$cpu_quota_cores, cpuset=$cpuset_cpu_count)"
  log_ok "Memory: target=${mem_total_mb}MB host=${host_mem_total_mb}MB cgroup_limit=${memory_limit_mb}MB"
  log_ok "Build profile: $build_profile"
  log_ok "NUMA nodes: ${numa_node_count:-unknown}; budget profile source=$budget_profile_source"
  if [[ -n "$fingerprint_caveats" ]]; then
    log_warn "Host fingerprint caveats: $fingerprint_caveats"
  fi
  log_ok "Environment fingerprint written"

  if [[ "$env_warnings" -gt 0 ]]; then
    log_warn "$env_warnings environment warning(s) — proceeding anyway"
  fi
else
  log_step "Skipping environment validation (--skip-env-check)"
fi

# ─── Phase 2: Build ─────────────────────────────────────────────────────────

if [[ "$SKIP_BUILD" -eq 0 ]]; then
  log_phase "Phase 2: Build (profile=$CARGO_PROFILE)"
  build_start=$(epoch_ms)

  # Build test binaries
  log_step "Building test binaries..."
  if VERGEN_GIT_SHA="$GIT_COMMIT_FULL" \
    VERGEN_GIT_DIRTY="$GIT_DIRTY" \
    "${CARGO_RUNNER_ARGS[@]}" test --no-run --profile "$CARGO_PROFILE" \
      --message-format=json-render-diagnostics \
      >"$BUILD_TESTS_JSONL_PATH" \
      2>"$OUTPUT_DIR/logs/build_tests.log"; then
    log_ok "Test binaries built"
  else
    log_warn "Test binary build had warnings (see logs/build_tests.log)"
  fi

  if [[ "$CARGO_RUNNER_MODE" == "rch" ]] \
    && (suite_selected "perf_budgets" || suite_selected "perf_regression"); then
    if python3 - \
      "$BUILD_TESTS_JSONL_PATH" \
      "$TARGET_DIR" \
      "$CARGO_PROFILE" \
      "$PERF_BUDGET_BINARY_ATTESTATION_PATH" \
      "$GIT_COMMIT_FULL" \
      "$GIT_DIRTY" <<'PY'
import hashlib
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

messages_path = Path(sys.argv[1])
target_dir = Path(sys.argv[2])
profile = sys.argv[3]
attestation_path = Path(sys.argv[4])
source_commit = sys.argv[5]
source_dirty = sys.argv[6] == "true"
candidates = []
for line in messages_path.read_text(encoding="utf-8").splitlines():
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    target = message.get("target", {})
    executable = message.get("executable")
    if (
        message.get("reason") == "compiler-artifact"
        and target.get("name") == "perf_budgets"
        and isinstance(executable, str)
        and executable
    ):
        local_candidate = target_dir / profile / "deps" / Path(executable).name
        if local_candidate.is_file() and local_candidate.stat().st_mode & 0o111:
            candidates.append(local_candidate.resolve())
if len(candidates) != 1:
    raise SystemExit(
        f"expected exactly one downloaded perf_budgets executable, found {len(candidates)}"
    )
binary_path = candidates[0]
payload = {
    "schema": "pi.perf.test_binary_attestation.v1",
    "generated_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    "source_commit": source_commit,
    "source_dirty": source_dirty,
    "cargo_profile": profile,
    "target_name": "perf_budgets",
    "binary_path": str(binary_path),
    "sha256": hashlib.sha256(binary_path.read_bytes()).hexdigest(),
}
attestation_path.write_text(
    json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
)
PY
    then
      log_ok "Attested current-build perf_budgets test binary"
    elif [[ "${PI_PERF_STRICT:-0}" == "1" ]]; then
      die "Failed to attest current-build perf_budgets test binary"
    else
      log_warn "Current-build perf_budgets binary attestation is unavailable"
    fi
  fi

  # Build criterion benches if needed
  if [[ "$SKIP_CRITERION" != "1" ]]; then
    log_step "Building criterion benchmarks..."
    for bench in "${!CRITERION_BENCHES[@]}"; do
      bench_name="${CRITERION_BENCHES[$bench]}"
      if "${CARGO_RUNNER_ARGS[@]}" bench --bench "$bench_name" --no-run --profile "$CARGO_PROFILE" 2>>"$OUTPUT_DIR/logs/build_benches.log"; then
        log_ok "Built bench: $bench_name"
      else
        log_warn "Build warning for bench: $bench_name"
      fi
    done
  fi

  if suite_selected "perf_budgets" || suite_selected "perf_regression"; then
    release_pi_built=0
    log_step "Building release pi binary for release-size gates..."
    if "${CARGO_RUNNER_ARGS[@]}" build --bin pi --release >"$OUTPUT_DIR/logs/build_release_pi.log" 2>&1; then
      release_pi_built=1
      log_ok "Release pi binary built: $TARGET_DIR/release/pi"
      write_binary_size_measurement_control "$TARGET_DIR/release/pi"
    elif [[ "${PI_PERF_STRICT:-0}" == "1" ]]; then
      die "Failed to build release pi binary required for binary-size gates (see logs/build_release_pi.log)"
    else
      log_warn "Failed to build release pi binary (see logs/build_release_pi.log); binary-size checks may return NO_DATA"
    fi

    if [[ "$release_pi_built" -eq 1 ]]; then
      idle_rss_raw_relative="perf/release_evidence/idle_memory_rss.raw.json"
      log_step "Sampling release pi interactive-idle RSS (N=5)..."
      if PI_IDLE_RSS_RAW_RELATIVE_PATH="$idle_rss_raw_relative" \
        PI_IDLE_RSS_SOURCE_COMMIT="$GIT_COMMIT_FULL" \
        PI_IDLE_RSS_SOURCE_DIRTY="$GIT_DIRTY" \
        PI_IDLE_RSS_CORRELATION_ID="$CORRELATION_ID" \
        PI_BENCH_BUILD_PROFILE=release \
        "${CARGO_RUNNER_ARGS[@]}" bench --bench system --profile release -- __idle_rss_control__ \
        >"$OUTPUT_DIR/logs/idle_memory_rss.log" 2>&1; then
        write_idle_rss_measurement_control \
          "$OUTPUT_DIR/logs/idle_memory_rss.log" \
          "$TARGET_DIR/release/pi"
      elif [[ "${PI_PERF_STRICT:-0}" == "1" ]]; then
        die "Failed to sample release pi idle RSS (see logs/idle_memory_rss.log)"
      else
        log_warn "Failed to sample release pi idle RSS (see logs/idle_memory_rss.log); idle-memory checks may return NO_DATA"
      fi
    else
      log_warn "Skipping idle-RSS sampling because this run did not build its release pi binary"
    fi
  fi

  build_end=$(epoch_ms)
  build_elapsed=$((build_end - build_start))
  log_ok "Build completed in ${build_elapsed}ms"
else
  log_step "Skipping build (--skip-build / PERF_SKIP_BUILD=1)"
fi

# ─── Phase 2b: Budget artifact preflight ────────────────────────────────────

if suite_selected "perf_budgets" || suite_selected "perf_regression"; then
  log_phase "Phase 2b: Budget Artifact Preflight"
  preflight_exit=0
  if run_budget_preflight "$PREFLIGHT_BEFORE_REFRESH_PATH"; then
    log_ok "Budget artifact preflight passed: results/$(basename "$PREFLIGHT_BEFORE_REFRESH_PATH")"
  else
    preflight_exit=$?
    log_warn "Budget artifact blockers written before budget summary refresh:"
    log_warn "  results/$(basename "$PREFLIGHT_BEFORE_REFRESH_PATH") (exit=$preflight_exit)"
    log_warn "The final artifact staging gate will remain blocked unless the required paths are refreshed."
  fi
fi

# ─── Phase 3: Execute suites ────────────────────────────────────────────────

log_phase "Phase 3: Execute Suites"

run_start=$(epoch_ms)
suite_pass=0
suite_fail=0
suite_skip=0
declare -a SUITE_RESULTS=()

validate_retrieved_extension_bench_jsonl() {
  local artifact_path="$1"
  local expected_profile="$2"
  local expected_commit="$3"
  local expected_correlation_id="$4"
  local expected_benchmark_run_id="$5"
  python3 - \
    "$artifact_path" \
    "$expected_profile" \
    "$expected_commit" \
    "$expected_correlation_id" \
    "$expected_benchmark_run_id" <<'PY'
import json
import sys
from pathlib import Path

artifact_path = Path(sys.argv[1])
expected_profile = sys.argv[2]
expected_commit = sys.argv[3]
expected_correlation_id = sys.argv[4]
expected_benchmark_run_id = sys.argv[5]
records = []
for line_number, line in enumerate(artifact_path.read_text(encoding="utf-8").splitlines(), 1):
    if not line.strip():
        continue
    try:
        record = json.loads(line)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"line {line_number}: invalid JSON: {exc}") from exc
    if not isinstance(record, dict):
        raise SystemExit(f"line {line_number}: record must be an object")
    if record.get("schema") != "pi.ext.rust_bench.v1":
        raise SystemExit(f"line {line_number}: unexpected schema")
    if record.get("runtime") != "pi_agent_rust":
        raise SystemExit(f"line {line_number}: unexpected runtime")
    if record.get("run_id") != expected_correlation_id:
        raise SystemExit(f"line {line_number}: run_id does not match current run")
    if record.get("correlation_id") != expected_correlation_id:
        raise SystemExit(f"line {line_number}: correlation_id does not match current run")
    if record.get("benchmark_run_id") != expected_benchmark_run_id:
        raise SystemExit(f"line {line_number}: benchmark_run_id does not match current invocation")
    if record.get("source_commit") != expected_commit:
        raise SystemExit(f"line {line_number}: source_commit does not match {expected_commit}")
    if record.get("source_dirty") is not False:
        raise SystemExit(f"line {line_number}: source_dirty must equal false")
    environment = record.get("env")
    if not isinstance(environment, dict) or environment.get("build_profile") != expected_profile:
        raise SystemExit(f"line {line_number}: build profile does not match {expected_profile}")
    if environment.get("git_commit") != expected_commit:
        raise SystemExit(f"line {line_number}: git commit does not match {expected_commit}")
    if environment.get("source_dirty") is not False:
        raise SystemExit(f"line {line_number}: env.source_dirty must equal false")
    if environment.get("executable_build_profile") != expected_profile:
        raise SystemExit(
            f"line {line_number}: executable build profile does not match {expected_profile}"
        )
    for field in (
        "executable_profile_verified",
        "build_fingerprint_verified",
        "build_profile_verified",
    ):
        if environment.get(field) is not True:
            raise SystemExit(f"line {line_number}: env.{field} must equal true")
    if environment.get("build_fingerprint_contract") != "cargo_build_fingerprint.v1":
        raise SystemExit(f"line {line_number}: unexpected build fingerprint contract")
    if (
        environment.get("compiled_profile_family") != "release"
        or environment.get("compiled_opt_level") != "3"
        or environment.get("compiled_debug") != "true"
    ):
        raise SystemExit(f"line {line_number}: compiled perf fingerprint is invalid")
    records.append(record)

if not records:
    raise SystemExit("extension benchmark JSONL contains no records")
if not any(
    record.get("scenario") == "cold_start"
    and isinstance(record.get("summary"), dict)
    and isinstance(record["summary"].get("count"), int)
    and record["summary"]["count"] > 0
    for record in records
):
    raise SystemExit("extension benchmark JSONL has no successful cold_start record")
PY
}

run_test_suite() {
  local suite_name="$1"
  local target_name="$2"
  local suite_start suite_end suite_elapsed exit_code
  local rch_target_subdir=""
  local benchmark_run_id=""

  log_step "Running suite: $suite_name (target=$target_name)"
  suite_start=$(epoch_ms)

  local result_dir="$OUTPUT_DIR/results/$suite_name"
  mkdir -p "$result_dir"

  exit_code=0
  if [[ "$CARGO_RUNNER_MODE" == "rch" && "$suite_name" == "perf_bench_harness" ]]; then
    # RCH 1.0.58 does not retrieve arbitrary CargoTest output. CargoNextest has
    # an explicit nextest/** target-dir sync contract, so write the benchmark
    # artifact beneath the worker's active CARGO_TARGET_DIR and require it to
    # arrive in the matching local target directory before crediting the suite.
    rch_target_subdir="nextest/pi-perf/$CORRELATION_ID/$suite_name"
    benchmark_run_id="${CORRELATION_ID}:${suite_start}:$$"
    local retrieved_result_dir="$TARGET_DIR/$rch_target_subdir"
    if ! verify_current_clean_source_identity "RCH extension benchmark precondition"; then
      exit_code=89
    elif [[ -e "$retrieved_result_dir/extension_bench.jsonl" \
      || -L "$retrieved_result_dir/extension_bench.jsonl" \
      || -e "$retrieved_result_dir/extension_bench_summary.md" \
      || -L "$retrieved_result_dir/extension_bench_summary.md" ]]; then
      log_fail "Refusing stale RCH extension benchmark artifacts at $rch_target_subdir"
      exit_code=87
    else
      BENCH_OUTPUT_TARGET_SUBDIR="$rch_target_subdir" \
      PI_BENCH_RUN_ID="$benchmark_run_id" \
      PERF_REGRESSION_OUTPUT="$result_dir" \
      PERF_RELEASE_BINARY_PATH="$TARGET_DIR/release/pi" \
      CI_CORRELATION_ID="$CORRELATION_ID" \
      VERGEN_GIT_SHA="$GIT_COMMIT_FULL" \
      VERGEN_GIT_DIRTY="$GIT_DIRTY" \
      RUST_TEST_THREADS="$PARALLELISM" \
      CARGO_BUILD_JOBS="$BUILD_JOBS" \
      PI_BENCH_BUILD_PROFILE="$CARGO_PROFILE" \
      RCH_REQUIRE_REMOTE=1 \
      RCH_QUIET=0 \
      RCH_VISIBILITY=summary \
        "${PERF_BENCH_RUNNER_ARGS[@]}" nextest run \
          --build-jobs "$BUILD_JOBS" \
          --test "$target_name" \
          --cargo-profile "$CARGO_PROFILE" \
          --test-threads 1 \
          --no-tests fail \
          -- bench_extension_scenarios --exact \
        >"$result_dir/stdout.log" 2>"$result_dir/stderr.log" \
        || exit_code=$?
    fi

    if [[ "$exit_code" -eq 0 ]] \
      && ! verify_current_clean_source_identity "RCH extension benchmark postcondition"; then
      exit_code=91
    fi

    if [[ "$exit_code" -eq 0 ]]; then
      if ! grep -Eqs \
        '^\[RCH\] remote [^[:space:]]+ \([^)]+\)$' \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "RCH extension benchmark has no remote-success marker"
        exit_code=92
      elif grep -Eqs '^\[RCH\] local( |$)' \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "RCH extension benchmark reported local execution"
        exit_code=93
      elif ! grep -Eqs \
        "^\\[RCH\\] clean-overlay receipt: base=$GIT_COMMIT_FULL overlay-fingerprint=[0-9a-f]{64}$" \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "RCH extension benchmark has no current-commit clean-overlay receipt"
        exit_code=94
      fi
    fi

    if [[ "$exit_code" -eq 0 \
      && -s "$retrieved_result_dir/extension_bench.jsonl" \
      && ! -L "$retrieved_result_dir/extension_bench.jsonl" ]]; then
      if validate_retrieved_extension_bench_jsonl \
        "$retrieved_result_dir/extension_bench.jsonl" \
        "$CARGO_PROFILE" \
        "$GIT_COMMIT_FULL" \
        "$CORRELATION_ID" \
        "$benchmark_run_id"; then
        if [[ -e "$result_dir/extension_bench.jsonl" \
          || -L "$result_dir/extension_bench.jsonl" ]]; then
          log_fail "Refusing preexisting accepted extension benchmark destination"
          exit_code=95
        else
          cp "$retrieved_result_dir/extension_bench.jsonl" "$result_dir/extension_bench.jsonl"
          if ! validate_retrieved_extension_bench_jsonl \
            "$result_dir/extension_bench.jsonl" \
            "$CARGO_PROFILE" \
            "$GIT_COMMIT_FULL" \
            "$CORRELATION_ID" \
            "$benchmark_run_id"; then
            log_fail "Accepted extension benchmark copy failed post-copy validation"
            exit_code=96
          fi
        fi
      else
        log_fail "RCH retrieved an invalid extension_bench.jsonl from $rch_target_subdir"
        exit_code=88
      fi
      if [[ "$exit_code" -eq 0 \
        && -s "$retrieved_result_dir/extension_bench_summary.md" \
        && ! -L "$retrieved_result_dir/extension_bench_summary.md" ]]; then
        cp "$retrieved_result_dir/extension_bench_summary.md" "$result_dir/extension_bench_summary.md"
      fi
    elif [[ "$exit_code" -eq 0 ]]; then
      log_fail "RCH completed $suite_name without retrieving extension_bench.jsonl from $rch_target_subdir"
      exit_code=86
    fi
  else
    BENCH_OUTPUT_DIR="$result_dir" \
    PERF_REGRESSION_OUTPUT="$result_dir" \
    PERF_RELEASE_BINARY_PATH="$TARGET_DIR/release/pi" \
    CI_CORRELATION_ID="$CORRELATION_ID" \
    VERGEN_GIT_SHA="$GIT_COMMIT_FULL" \
    VERGEN_GIT_DIRTY="$GIT_DIRTY" \
    RUST_TEST_THREADS="$PARALLELISM" \
      "${CARGO_RUNNER_ARGS[@]}" test --test "$target_name" --profile "$CARGO_PROFILE" -- --nocapture \
      >"$result_dir/stdout.log" 2>"$result_dir/stderr.log" \
      || exit_code=$?
  fi

  suite_end=$(epoch_ms)
  suite_elapsed=$((suite_end - suite_start))

  local status
  if [[ "$exit_code" -eq 0 ]]; then
    status="pass"
    suite_pass=$((suite_pass + 1))
    log_ok "$suite_name passed (${suite_elapsed}ms)"
  else
    status="fail"
    suite_fail=$((suite_fail + 1))
    log_fail "$suite_name failed (exit=$exit_code, ${suite_elapsed}ms)"
  fi

  # Write per-suite result record
  # Some suite targets may clean BENCH_OUTPUT_DIR on failure; ensure the sink exists.
  mkdir -p "$result_dir"
  cat > "$result_dir/result.json" <<EOF
{
  "schema": "pi.perf.suite_result.v1",
  "suite_name": "$suite_name",
  "target": "$target_name",
  "status": "$status",
  "exit_code": $exit_code,
  "elapsed_ms": $suite_elapsed,
  "correlation_id": "$CORRELATION_ID",
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "profile": "$CARGO_PROFILE"
}
EOF

  SUITE_RESULTS+=("{\"suite\":\"$suite_name\",\"status\":\"$status\",\"exit_code\":$exit_code,\"elapsed_ms\":$suite_elapsed}")
}

run_criterion_bench() {
  local suite_name="$1"
  local bench_name="$2"
  local suite_start suite_end suite_elapsed exit_code

  log_step "Running criterion bench: $suite_name (bench=$bench_name)"
  suite_start=$(epoch_ms)

  local result_dir="$OUTPUT_DIR/results/$suite_name"
  mkdir -p "$result_dir"

  exit_code=0
  "${CARGO_RUNNER_ARGS[@]}" bench --bench "$bench_name" --profile "$CARGO_PROFILE" \
    >"$result_dir/stdout.log" 2>"$result_dir/stderr.log" \
    || exit_code=$?

  suite_end=$(epoch_ms)
  suite_elapsed=$((suite_end - suite_start))

  local status
  if [[ "$exit_code" -eq 0 ]]; then
    status="pass"
    suite_pass=$((suite_pass + 1))
    log_ok "$suite_name passed (${suite_elapsed}ms)"
  else
    status="fail"
    suite_fail=$((suite_fail + 1))
    log_fail "$suite_name failed (exit=$exit_code, ${suite_elapsed}ms)"
  fi

  # Copy criterion output if it exists
  local criterion_dir="$TARGET_DIR/criterion/$bench_name"
  if [[ -d "$criterion_dir" ]]; then
    cp -r "$criterion_dir" "$result_dir/criterion/" 2>/dev/null || true
  fi

  if [[ "$suite_name" == "criterion_extensions" ]]; then
    write_cold_load_measurement_control "$result_dir" "$exit_code"
  fi

  cat > "$result_dir/result.json" <<EOF
{
  "schema": "pi.perf.suite_result.v1",
  "suite_name": "$suite_name",
  "target": "$bench_name",
  "kind": "criterion",
  "status": "$status",
  "exit_code": $exit_code,
  "elapsed_ms": $suite_elapsed,
  "correlation_id": "$CORRELATION_ID",
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "profile": "$CARGO_PROFILE"
}
EOF

  SUITE_RESULTS+=("{\"suite\":\"$suite_name\",\"status\":\"$status\",\"exit_code\":$exit_code,\"elapsed_ms\":$suite_elapsed}")
}

# Execute each selected suite. The budget consumer runs last so a full profile
# cannot inspect ambient Criterion files before this run has emitted their
# hash-bound environment controls.
deferred_perf_budgets=false
for suite in "${SELECTED_SUITES[@]}"; do
  if [[ "$suite" == "perf_budgets" ]]; then
    deferred_perf_budgets=true
    continue
  fi
  if [[ -n "${SUITE_TARGETS[$suite]+x}" ]]; then
    run_test_suite "$suite" "${SUITE_TARGETS[$suite]}"
  elif [[ -n "${CRITERION_BENCHES[$suite]+x}" ]]; then
    run_criterion_bench "$suite" "${CRITERION_BENCHES[$suite]}"
  else
    log_warn "Unknown suite: $suite (skipping)"
    suite_skip=$((suite_skip + 1))
  fi
done

if [[ "$deferred_perf_budgets" == "true" ]]; then
  run_test_suite "perf_budgets" "${SUITE_TARGETS[perf_budgets]}"
fi

run_end=$(epoch_ms)
run_elapsed=$((run_end - run_start))

# ─── Phase 4: Collect JSONL artifacts ────────────────────────────────────────

log_phase "Phase 4: Collect Artifacts"

artifact_count=0

# Collect JSONL outputs from standard locations
collect_jsonl() {
  local src="$1"
  local dst_name="$2"
  if [[ -f "$src" ]]; then
    cp "$src" "$OUTPUT_DIR/results/$dst_name"
    artifact_count=$((artifact_count + 1))
    log_ok "Collected: $dst_name ($(wc -l < "$src") records)"
  fi
}

# Standard JSONL output paths
collect_jsonl "$OUTPUT_DIR/results/perf_bench_harness/extension_bench.jsonl" "extension_bench.jsonl"
collect_jsonl "$TARGET_DIR/perf/ext_bench_harness.jsonl" "ext_bench_harness.jsonl"
collect_jsonl "$TARGET_DIR/perf/scenario_runner.jsonl" "scenario_runner.jsonl"
collect_jsonl "$TARGET_DIR/perf/pijs_workload.jsonl" "pijs_workload.jsonl"
collect_jsonl "$TARGET_DIR/perf/legacy_extension_workloads.jsonl" "legacy_extension_workloads.jsonl"
collect_jsonl "$TARGET_DIR/perf/$CARGO_PROFILE/pgo_pipeline_events.jsonl" "pgo_pipeline_events.jsonl"

if [[ -f "$TARGET_DIR/perf/ext_bench_harness_report.json" ]]; then
  cp "$TARGET_DIR/perf/ext_bench_harness_report.json" "$OUTPUT_DIR/results/ext_bench_harness_report.json"
  artifact_count=$((artifact_count + 1))
  log_ok "Collected: ext_bench_harness_report.json"
fi

if [[ -d "$TARGET_DIR/perf/$CARGO_PROFILE" ]]; then
  pgo_compare_dir="$OUTPUT_DIR/results/pgo_comparison"
  mkdir -p "$pgo_compare_dir"
  while IFS= read -r -d '' pgo_json; do
    cp "$pgo_json" "$pgo_compare_dir/" 2>/dev/null || true
    artifact_count=$((artifact_count + 1))
    log_ok "Collected PGO comparison artifact: $(basename "$pgo_json")"
  done < <(find "$TARGET_DIR/perf/$CARGO_PROFILE" -maxdepth 1 -type f -name "pgo_delta_*.json" -print0 2>/dev/null)
fi

# Check per-suite result directories for additional JSONL
for suite in "${SELECTED_SUITES[@]}"; do
  suite_dir="$OUTPUT_DIR/results/$suite"
  if [[ -d "$suite_dir" ]]; then
    while IFS= read -r -d '' jsonl_file; do
      basename_file="$(basename "$jsonl_file")"
      if [[ "$basename_file" != "stdout.log" && "$basename_file" != "stderr.log" ]]; then
        artifact_count=$((artifact_count + 1))
      fi
    done < <(find "$suite_dir" -name "*.jsonl" -print0 2>/dev/null)
  fi
done

# Collect benchmark reports from tests/perf/reports
if [[ -d "$PROJECT_ROOT/tests/perf/reports" ]]; then
  cp -r "$PROJECT_ROOT/tests/perf/reports" "$OUTPUT_DIR/results/perf_reports/" 2>/dev/null || true
  log_ok "Collected perf reports directory"
fi

if [[ -f "$PREFLIGHT_BEFORE_REFRESH_PATH" ]]; then
  artifact_count=$((artifact_count + 1))
  log_ok "Collected: $(basename "$PREFLIGHT_BEFORE_REFRESH_PATH")"
fi

log_ok "Artifacts collected before derived finalization: $artifact_count"

# ─── Phase 5: Generate manifest ─────────────────────────────────────────────

log_phase "Phase 5: Generate Run Manifest"

# Build suite_results JSON array
suite_results_json="["
first=true
for result in "${SUITE_RESULTS[@]}"; do
  if [[ "$first" == "true" ]]; then
    first=false
  else
    suite_results_json+=","
  fi
  suite_results_json+="$result"
done
suite_results_json+="]"

cat > "$OUTPUT_DIR/manifest.json" <<EOF
{
  "schema": "pi.perf.run_manifest.v1",
  "version": "1.0.0",
  "bead_id": "bd-3ar8v.1.8",
  "correlation_id": "$CORRELATION_ID",
  "timestamp": "$TIMESTAMP",
  "generated_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "git_commit": "$GIT_COMMIT",
  "git_dirty": $GIT_DIRTY,
  "profile": "$PROFILE",
  "cargo_profile": "$CARGO_PROFILE",
  "parallelism": $PARALLELISM,
  "build_jobs": $BUILD_JOBS,
  "run_summary": {
    "total_suites": $((suite_pass + suite_fail + suite_skip)),
    "passed": $suite_pass,
    "failed": $suite_fail,
    "skipped": $suite_skip,
    "elapsed_ms": $run_elapsed,
    "artifact_count": $artifact_count
  },
  "artifact_staging": {
    "schema": "pi.perf.artifact_staging_manifest.v1",
    "manifest_path": "$STAGING_MANIFEST_PATH",
    "pre_refresh_report_path": "$PREFLIGHT_BEFORE_REFRESH_PATH",
    "post_run_report_path": "$PREFLIGHT_AFTER_RUN_PATH",
    "evidence_cache_dir": "$EVIDENCE_CACHE_DIR",
    "evidence_cache_ttl_hours": $EVIDENCE_CACHE_TTL_HOURS,
    "status": "$ARTIFACT_STAGING_STATUS",
    "missing_required_count": $ARTIFACT_STAGING_MISSING_REQUIRED,
    "stale_required_count": $ARTIFACT_STAGING_STALE_REQUIRED,
    "blocker_count": $ARTIFACT_STAGING_BLOCKERS
  },
  "suite_results": $suite_results_json,
  "contract_refs": {
    "logging_contract": "pi.test.evidence_logging_contract.v1",
    "evidence_contract": "pi.qa.evidence_contract.v1",
    "bench_protocol": "pi.bench.protocol.v1",
    "sli_matrix": "pi.perf.sli_ux_matrix.v1",
    "pgo_pipeline": "pi.perf.pgo_pipeline_summary.v1",
    "extension_stratification": "pi.perf.extension_benchmark_stratification.v1",
    "cross_env_variance_diagnosis": "pi.perf.cross_env_variance_diagnosis.v1",
    "phase1_matrix_validation": "pi.perf.phase1_matrix_validation.v1",
    "parameter_sweeps": "pi.perf.parameter_sweeps.v1",
    "opportunity_matrix": "pi.perf.opportunity_matrix.v1"
  },
  "output_dir": "$OUTPUT_DIR"
}
EOF

log_ok "Manifest written: manifest.json"

# ─── Phase 5b: Baseline Variance/Confidence Artifact ────────────────────────

log_phase "Phase 5b: Baseline Variance/Confidence"

BASELINE_CONFIDENCE_PATH="$OUTPUT_DIR/results/baseline_variance_confidence.json"
if OUTPUT_DIR="$OUTPUT_DIR" \
  PROJECT_ROOT="$PROJECT_ROOT" \
  CORRELATION_ID="$CORRELATION_ID" \
  TIMESTAMP="$TIMESTAMP" \
  BASELINE_CONFIDENCE_PATH="$BASELINE_CONFIDENCE_PATH" \
  python3 - <<'PY'
import hashlib
import json
import math
import os
from datetime import datetime, timezone
from pathlib import Path

output_dir = Path(os.environ["OUTPUT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
correlation_id = os.environ["CORRELATION_ID"]
timestamp = os.environ["TIMESTAMP"]
baseline_confidence_path = Path(os.environ["BASELINE_CONFIDENCE_PATH"])

manifest_path = output_dir / "manifest.json"
env_path = output_dir / "env_fingerprint.json"
perf_sli_path = project_root / "docs" / "perf_sli_matrix.json"
scenario_matrix_path = project_root / "docs" / "e2e_scenario_matrix.json"


def load_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


manifest = load_json(manifest_path)
env = load_json(env_path) if env_path.exists() else {}
perf_sli = load_json(perf_sli_path)
scenario_matrix = load_json(scenario_matrix_path)

suite_results = manifest.get("suite_results", [])
if not isinstance(suite_results, list):
    suite_results = []
suite_result_by_name = {
    str(entry.get("suite", "")).strip(): entry
    for entry in suite_results
    if isinstance(entry, dict) and str(entry.get("suite", "")).strip()
}

scenario_rows = scenario_matrix.get("rows", [])
if not isinstance(scenario_rows, list):
    scenario_rows = []
scenario_by_workflow = {
    str(row.get("workflow_id", "")).strip(): row
    for row in scenario_rows
    if isinstance(row, dict) and str(row.get("workflow_id", "")).strip()
}

partition_requirements_raw = (
    perf_sli.get("reporting_contract", {})
    .get("scenario_partition_requirements", [])
)
if not isinstance(partition_requirements_raw, list):
    partition_requirements_raw = []
required_partition_tags_raw = (
    perf_sli.get("reporting_contract", {})
    .get("required_partition_tags", [])
)
if not isinstance(required_partition_tags_raw, list):
    required_partition_tags_raw = []
required_partition_tags = []
for partition in required_partition_tags_raw:
    partition_tag = str(partition).strip()
    if partition_tag and partition_tag not in required_partition_tags:
        required_partition_tags.append(partition_tag)
if not required_partition_tags:
    required_partition_tags = ["matched-state", "realistic"]
required_partition_tag_set = set(required_partition_tags)

partitions_by_workflow = {}
for row in partition_requirements_raw:
    if not isinstance(row, dict):
        continue
    workflow_id = str(row.get("workflow_id", "")).strip()
    required_partitions = row.get("required_partitions", [])
    if not workflow_id or not isinstance(required_partitions, list):
        continue
    partitions = []
    for partition in required_partitions:
        partition_tag = str(partition).strip()
        if not partition_tag:
            continue
        if partition_tag not in required_partition_tag_set:
            raise ValueError(
                f"workflow {workflow_id} defines unsupported partition tag: {partition_tag}"
            )
        if partition_tag not in partitions:
            partitions.append(partition_tag)
    if partitions:
        partitions_by_workflow[workflow_id] = partitions

workflow_sli_mapping = perf_sli.get("workflow_sli_mapping", [])
if not isinstance(workflow_sli_mapping, list):
    workflow_sli_mapping = []

run_id = str(manifest.get("timestamp", timestamp))
environment_fingerprint_hash = str(env.get("config_hash", "unknown"))

records = []

for mapping in workflow_sli_mapping:
    if not isinstance(mapping, dict):
        continue

    workflow_id = str(mapping.get("workflow_id", "")).strip()
    sli_ids = mapping.get("sli_ids", [])
    if not workflow_id or not isinstance(sli_ids, list):
        continue

    scenario_row = scenario_by_workflow.get(workflow_id, {})
    suite_ids = scenario_row.get("suite_ids", [])
    if not isinstance(suite_ids, list):
        suite_ids = []
    suite_ids = [str(suite_id).strip() for suite_id in suite_ids if str(suite_id).strip()]

    sample_values = []
    for suite_id in suite_ids:
        suite_result = suite_result_by_name.get(suite_id)
        if not isinstance(suite_result, dict):
            continue
        if str(suite_result.get("status", "")).strip().lower() != "pass":
            continue
        elapsed_ms = suite_result.get("elapsed_ms")
        if isinstance(elapsed_ms, (int, float)):
            sample_values.append(float(elapsed_ms))

    sample_count = len(sample_values)
    mean_ms = None
    variance_ms2 = None
    stddev_ms = None
    ci95_lower_ms = None
    ci95_upper_ms = None

    if sample_count > 0:
        mean_ms = sum(sample_values) / sample_count
        if sample_count > 1:
            variance_ms2 = sum((value - mean_ms) ** 2 for value in sample_values) / sample_count
            stddev_ms = math.sqrt(variance_ms2)
            half_width = 1.96 * stddev_ms / math.sqrt(sample_count)
        else:
            variance_ms2 = 0.0
            stddev_ms = 0.0
            half_width = 0.0
        ci95_lower_ms = max(0.0, mean_ms - half_width)
        ci95_upper_ms = mean_ms + half_width

    if sample_count >= 8:
        confidence = "high"
    elif sample_count >= 4:
        confidence = "medium"
    else:
        confidence = "low"

    evidence_state = "measured" if sample_count > 0 else "no_data"
    explicit_partitions = partitions_by_workflow.get(workflow_id)
    if explicit_partitions is None:
        required_partitions = list(required_partition_tags)
    else:
        missing_partitions = required_partition_tag_set.difference(explicit_partitions)
        if missing_partitions:
            missing_csv = ", ".join(sorted(missing_partitions))
            raise ValueError(
                f"workflow {workflow_id} missing required workload partitions: {missing_csv}"
            )
        required_partitions = [
            partition
            for partition in required_partition_tags
            if partition in explicit_partitions
        ]

    lineage_source = {
        "workflow_id": workflow_id,
        "suite_ids": suite_ids,
        "sample_values_ms": sample_values,
        "required_partitions": required_partitions,
    }
    dataset_hash = hashlib.sha256(
        json.dumps(lineage_source, sort_keys=True, separators=(",", ":")).encode("utf-8")
    ).hexdigest()

    scenario_metadata = {
        "workflow_id": workflow_id,
        "workflow_class": str(scenario_row.get("workflow_class", "unknown")),
        "suite_ids": suite_ids,
        "vcr_mode": str(scenario_row.get("vcr_mode", "unknown")),
        "scenario_owner": str(scenario_row.get("owner", "unknown")),
    }

    for partition in required_partitions:
        for sli_id in sli_ids:
            canonical_sli_id = str(sli_id).strip()
            if not canonical_sli_id:
                continue
            records.append(
                {
                    "run_id": run_id,
                    "correlation_id": correlation_id,
                    "scenario_id": workflow_id,
                    "workload_partition": partition,
                    "scenario_metadata": scenario_metadata,
                    "sli_id": canonical_sli_id,
                    "sample_count": sample_count,
                    "mean_ms": mean_ms,
                    "variance_ms2": variance_ms2,
                    "stddev_ms": stddev_ms,
                    "ci95_lower_ms": ci95_lower_ms,
                    "ci95_upper_ms": ci95_upper_ms,
                    "confidence": confidence,
                    "evidence_state": evidence_state,
                    "lineage": {
                        "dataset_hash": dataset_hash,
                        "run_id_lineage": [run_id, correlation_id],
                        "environment_fingerprint_hash": environment_fingerprint_hash,
                        "source_manifest_path": str(manifest_path),
                    },
                }
            )

confidence_counts = {"high": 0, "medium": 0, "low": 0}
for record in records:
    label = str(record.get("confidence", "low"))
    confidence_counts[label] = confidence_counts.get(label, 0) + 1

payload = {
    "schema": "pi.perf.baseline_variance_confidence.v1",
    "bead_id": "bd-3ar8v.1.5",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "run_id": run_id,
    "correlation_id": correlation_id,
    "source_manifest_path": str(manifest_path),
    "source_env_fingerprint_path": str(env_path) if env_path.exists() else None,
    "records": records,
    "summary": {
        "record_count": len(records),
        "scenario_count": len({record["scenario_id"] for record in records}),
        "sli_count": len({record["sli_id"] for record in records}),
        "confidence_counts": confidence_counts,
    },
}

baseline_confidence_path.parent.mkdir(parents=True, exist_ok=True)
baseline_confidence_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

manifest["baseline_variance_confidence"] = {
    "schema": "pi.perf.baseline_variance_confidence.v1",
    "path": str(baseline_confidence_path),
    "record_count": payload["summary"]["record_count"],
    "scenario_count": payload["summary"]["scenario_count"],
    "sli_count": payload["summary"]["sli_count"],
}
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY
then
  artifact_count=$((artifact_count + 1))
  log_ok "Baseline variance/confidence written: results/baseline_variance_confidence.json"
else
  die "Failed to generate baseline variance/confidence artifact"
fi

# ─── Phase 5c: PGO pipeline summary ────────────────────────────────────────

log_phase "Phase 5c: PGO Pipeline Summary"

PGO_SUMMARY_PATH="$OUTPUT_DIR/results/pgo_pipeline_summary.json"
if OUTPUT_DIR="$OUTPUT_DIR" \
  PROJECT_ROOT="$PROJECT_ROOT" \
  CORRELATION_ID="$CORRELATION_ID" \
  TIMESTAMP="$TIMESTAMP" \
  PGO_MODE="$PGO_MODE" \
  PGO_PROFILE_DATA="$PGO_PROFILE_DATA" \
  PGO_ALLOW_FALLBACK="$PGO_ALLOW_FALLBACK" \
  PGO_SUMMARY_PATH="$PGO_SUMMARY_PATH" \
  python3 - <<'PY'
import json
import os
from datetime import datetime, timezone
from pathlib import Path

output_dir = Path(os.environ["OUTPUT_DIR"])
correlation_id = os.environ["CORRELATION_ID"]
timestamp = os.environ["TIMESTAMP"]
pgo_mode_requested = os.environ["PGO_MODE"]
pgo_profile_data = os.environ["PGO_PROFILE_DATA"]
pgo_allow_fallback = os.environ["PGO_ALLOW_FALLBACK"]
pgo_summary_path = Path(os.environ["PGO_SUMMARY_PATH"])

manifest_path = output_dir / "manifest.json"
events_path = output_dir / "results" / "pgo_pipeline_events.jsonl"
comparison_dir = output_dir / "results" / "pgo_comparison"


def load_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def load_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows = []
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        line = line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError as error:
            rows.append(
                {
                    "__lineage_parse_error": "invalid_json",
                    "line_number": line_number,
                    "detail": str(error),
                }
            )
            continue
        if isinstance(payload, dict):
            rows.append(payload)
        else:
            rows.append(
                {
                    "__lineage_parse_error": "non_object_json",
                    "line_number": line_number,
                }
            )
    return rows


manifest = load_json(manifest_path)
events = load_jsonl(events_path)

comparison_artifacts = []
if comparison_dir.exists():
    for path in sorted(comparison_dir.glob("pgo_delta_*.json")):
        comparison_artifacts.append(str(path))

latest_mode_effective = "off"
profile_data_state = "not_requested"
fallback_reasons = []
for event in events:
    mode_effective = str(event.get("pgo_mode_effective", "")).strip()
    if mode_effective:
        latest_mode_effective = mode_effective
    state = str(event.get("profile_data_state", "")).strip()
    if state:
        profile_data_state = state
    fallback_reason = str(event.get("fallback_reason", "")).strip()
    if fallback_reason:
        fallback_reasons.append(fallback_reason)

profile_path = Path(pgo_profile_data)
if profile_data_state == "not_requested":
    if pgo_mode_requested in {"use", "train", "compare"}:
        if not profile_path.exists():
            profile_data_state = "missing"
        elif profile_path.stat().st_size == 0:
            profile_data_state = "corrupt"
        else:
            profile_data_state = "present"

if pgo_mode_requested == "off":
    latest_mode_effective = "off"
    profile_data_state = "not_requested"

fallback_triggered = len(fallback_reasons) > 0 or latest_mode_effective == "baseline_fallback"

summary = {
    "schema": "pi.perf.pgo_pipeline_summary.v1",
    "bead_id": "bd-3ar8v.5.2",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "run_id": str(manifest.get("timestamp", timestamp)),
    "correlation_id": correlation_id,
    "pgo_mode_requested": pgo_mode_requested,
    "pgo_mode_effective": latest_mode_effective,
    "profile_data_path": pgo_profile_data,
    "profile_data_state": profile_data_state,
    "fallback": {
        "allowed": pgo_allow_fallback in {"1", "true", "TRUE"},
        "triggered": fallback_triggered,
        "reasons": sorted(set(fallback_reasons)),
    },
    "events_path": str(events_path),
    "event_count": len(events),
    "comparison_artifacts": comparison_artifacts,
    "lineage": {
        "run_id_lineage": [str(manifest.get("timestamp", timestamp)), correlation_id],
        "source_manifest_path": str(manifest_path),
    },
}

pgo_summary_path.parent.mkdir(parents=True, exist_ok=True)
pgo_summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

manifest["pgo_pipeline_summary"] = {
    "schema": "pi.perf.pgo_pipeline_summary.v1",
    "path": str(pgo_summary_path),
    "event_count": len(events),
    "comparison_artifact_count": len(comparison_artifacts),
}
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY
then
  artifact_count=$((artifact_count + 1))
  log_ok "PGO pipeline summary written: results/pgo_pipeline_summary.json"
else
  die "Failed to generate PGO pipeline summary artifact"
fi

# ─── Phase 5d: Extension benchmark stratification ───────────────────────────

log_phase "Phase 5d: Extension Benchmark Stratification"

STRATIFICATION_PATH="$OUTPUT_DIR/results/extension_benchmark_stratification.json"
if OUTPUT_DIR="$OUTPUT_DIR" \
  PROJECT_ROOT="$PROJECT_ROOT" \
  CORRELATION_ID="$CORRELATION_ID" \
  GIT_COMMIT_FULL="$GIT_COMMIT_FULL" \
  GIT_DIRTY="$GIT_DIRTY" \
  TIMESTAMP="$TIMESTAMP" \
  STRATIFICATION_PATH="$STRATIFICATION_PATH" \
  python3 - <<'PY'
import hashlib
import json
import os
import re
from datetime import datetime, timedelta, timezone
from pathlib import Path

output_dir = Path(os.environ["OUTPUT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
correlation_id = os.environ["CORRELATION_ID"]
source_commit = os.environ["GIT_COMMIT_FULL"]
source_dirty = os.environ["GIT_DIRTY"] == "true"
timestamp = os.environ["TIMESTAMP"]
run_started_at = datetime.strptime(timestamp, "%Y%m%dT%H%M%SZ").replace(
    tzinfo=timezone.utc
)
source_clock_skew = timedelta(seconds=120)
stratification_path = Path(os.environ["STRATIFICATION_PATH"])

manifest_path = output_dir / "manifest.json"
baseline_path = output_dir / "results" / "baseline_variance_confidence.json"
scenario_runner_path = output_dir / "results" / "scenario_runner.jsonl"
workload_path = output_dir / "results" / "pijs_workload.jsonl"
extension_bench_path = output_dir / "results" / "extension_bench.jsonl"
ext_bench_path = output_dir / "results" / "ext_bench_harness.jsonl"
ext_bench_report_path = output_dir / "results" / "ext_bench_harness_report.json"
legacy_path = output_dir / "results" / "legacy_extension_workloads.jsonl"
perf_comparison_path = output_dir / "results" / "perf_reports" / "perf_comparison.json"
perf_sli_path = project_root / "docs" / "perf_sli_matrix.json"


def load_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def load_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows: list[dict] = []
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        line = line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError as error:
            rows.append(
                {
                    "__lineage_parse_error": "invalid_json",
                    "line_number": line_number,
                    "detail": str(error),
                }
            )
            continue
        if isinstance(payload, dict):
            rows.append(payload)
        else:
            rows.append(
                {
                    "__lineage_parse_error": "non_object_json",
                    "line_number": line_number,
                }
            )
    return rows


def parse_record_timestamp(record: dict):
    raw = record.get("timestamp", record.get("generated_at"))
    if not isinstance(raw, str) or not raw.strip():
        return None
    normalized = raw.strip()
    if normalized.endswith("Z"):
        normalized = f"{normalized[:-1]}+00:00"
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.astimezone(timezone.utc)


def parse_float(value):
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        match = re.search(r"-?\d+(?:\.\d+)?", value)
        if match:
            return float(match.group(0))
    return None


def mean(values: list[float]):
    if not values:
        return None
    return sum(values) / float(len(values))


def suite_status(name: str, suite_map: dict[str, dict]) -> str:
    row = suite_map.get(name)
    if isinstance(row, dict):
        status = str(row.get("status", "")).strip().lower()
        return status if status else "unknown"
    return "missing"


def suite_log_paths(name: str) -> dict[str, str]:
    suite_dir = output_dir / "results" / name
    return {
        "stdout": str(suite_dir / "stdout.log"),
        "stderr": str(suite_dir / "stderr.log"),
    }


manifest = load_json(manifest_path)
run_id = str(manifest.get("timestamp", timestamp))
suite_results = manifest.get("suite_results", [])
if not isinstance(suite_results, list):
    suite_results = []
suite_result_by_name = {
    str(row.get("suite", "")).strip(): row
    for row in suite_results
    if isinstance(row, dict) and str(row.get("suite", "")).strip()
}

def admit_dataset(path: Path, records: list[dict], correlation_field: str, required: bool):
    accepted = []
    rejected = []
    for index, record in enumerate(records):
        observed_correlation = record.get(correlation_field)
        observed_commit = record.get("source_commit")
        observed_dirty = record.get("source_dirty")
        observed_timestamp = parse_record_timestamp(record)
        reasons = []
        if record.get("__lineage_parse_error"):
            reasons.append(str(record["__lineage_parse_error"]))
        if observed_correlation != correlation_id:
            reasons.append("correlation_id_mismatch")
        if record.get("run_id") != correlation_id:
            reasons.append("run_id_mismatch")
        if observed_commit != source_commit:
            reasons.append("source_commit_mismatch")
        if observed_dirty is not False:
            reasons.append("source_dirty_not_false")
        if observed_timestamp is None:
            reasons.append("missing_or_invalid_timestamp")
        elif observed_timestamp < run_started_at - source_clock_skew:
            reasons.append("timestamp_before_run_start")
        elif observed_timestamp > datetime.now(timezone.utc) + source_clock_skew:
            reasons.append("timestamp_in_future")
        if reasons:
            rejected.append({"record_index": index, "reasons": reasons})
        else:
            accepted.append(record)
    digest = None
    if path.is_file():
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
    return accepted, {
        "path": str(path),
        "sha256": digest,
        "required": required,
        "correlation_field": correlation_field,
        "expected_correlation_id": correlation_id,
        "expected_source_commit": source_commit,
        "expected_min_timestamp": (
            run_started_at - source_clock_skew
        ).isoformat().replace("+00:00", "Z"),
        "accepted_record_count": len(accepted),
        "rejected_record_count": len(rejected),
        "rejections": rejected,
    }


scenario_runner_records, scenario_dataset = admit_dataset(
    scenario_runner_path,
    load_jsonl(scenario_runner_path),
    "orchestration_correlation_id",
    True,
)
workload_records, workload_dataset = admit_dataset(
    workload_path, load_jsonl(workload_path), "correlation_id", True
)
extension_bench_records, extension_bench_dataset = admit_dataset(
    extension_bench_path,
    load_jsonl(extension_bench_path),
    "correlation_id",
    "perf_bench_harness" in suite_result_by_name,
)
ext_bench_records, ext_bench_dataset = admit_dataset(
    ext_bench_path, load_jsonl(ext_bench_path), "correlation_id", False
)
legacy_records, legacy_dataset = admit_dataset(
    legacy_path, load_jsonl(legacy_path), "correlation_id", True
)
source_datasets = [
    scenario_dataset,
    workload_dataset,
    extension_bench_dataset,
    ext_bench_dataset,
    legacy_dataset,
]

comparison_rows = []
if perf_comparison_path.exists():
    comparison_payload = load_json(perf_comparison_path)
    rows = comparison_payload.get("rows", [])
    if isinstance(rows, list):
        comparison_rows = [row for row in rows if isinstance(row, dict)]

# ── Absolute metrics by layer ───────────────────────────────────────────────

cold_samples_ms: list[float] = []
for record in ext_bench_records:
    if str(record.get("scenario", "")).strip() != "cold_load":
        continue
    if record.get("success") is False:
        continue
    stats = record.get("stats", {})
    if not isinstance(stats, dict):
        continue
    p95_us = parse_float(stats.get("p95_us"))
    if p95_us is not None:
        cold_samples_ms.append(p95_us / 1000.0)

if not cold_samples_ms:
    for record in scenario_runner_records:
        if str(record.get("scenario", "")).strip() != "cold_start":
            continue
        stats = record.get("stats", {})
        if not isinstance(stats, dict):
            continue
        p95_ms = parse_float(stats.get("p95_ms"))
        if p95_ms is not None:
            cold_samples_ms.append(p95_ms)

if not cold_samples_ms:
    for record in extension_bench_records:
        if str(record.get("scenario", "")).strip() != "cold_start":
            continue
        summary = record.get("summary", {})
        if not isinstance(summary, dict):
            continue
        p95_ms = parse_float(summary.get("p95_ms"))
        if p95_ms is not None:
            cold_samples_ms.append(p95_ms)

per_call_samples_us: list[float] = []
for record in scenario_runner_records:
    scenario = str(record.get("scenario", "")).strip()
    if scenario not in {"tool_call", "event_dispatch"}:
        continue
    per_call_us = parse_float(record.get("per_call_us"))
    if per_call_us is not None:
        per_call_samples_us.append(per_call_us)

if not per_call_samples_us:
    for record in extension_bench_records:
        scenario = str(record.get("scenario", "")).strip()
        if scenario not in {"tool_call", "event_hook"}:
            continue
        per_call_us = parse_float(record.get("per_call_us"))
        if per_call_us is not None:
            per_call_samples_us.append(per_call_us)

if not per_call_samples_us:
    for record in workload_records:
        per_call_us = parse_float(record.get("per_call_us"))
        if per_call_us is not None:
            per_call_samples_us.append(per_call_us)

full_e2e_samples_ms: list[float] = []
perf_regression_row = suite_result_by_name.get("perf_regression")
if isinstance(perf_regression_row, dict):
    elapsed_ms = parse_float(perf_regression_row.get("elapsed_ms"))
    if elapsed_ms is not None:
        full_e2e_samples_ms.append(elapsed_ms)
for record in workload_records:
    elapsed_ms = parse_float(record.get("elapsed_ms"))
    if elapsed_ms is not None:
        full_e2e_samples_ms.append(elapsed_ms)

cold_abs_ms = mean(cold_samples_ms)
per_call_abs_us = mean(per_call_samples_us)
full_e2e_abs_ms = mean(full_e2e_samples_ms)

# ── Relative ratios (Rust vs Node/Bun) by layer ────────────────────────────

def comparison_row(metric_substr: str, category_substr: str | None = None):
    metric_substr = metric_substr.lower()
    category_substr = category_substr.lower() if category_substr else None
    for row in comparison_rows:
        metric = str(row.get("metric", "")).lower()
        category = str(row.get("category", "")).lower()
        if metric_substr in metric and (
            category_substr is None or category_substr in category
        ):
            return row
    return None


def extract_ratio_from_comparison_row(row):
    if not isinstance(row, dict):
        return None
    rust_value = parse_float(row.get("rust_value"))
    legacy_value = parse_float(row.get("legacy_value"))
    if rust_value is not None and legacy_value and legacy_value > 0:
        return rust_value / legacy_value
    metric = str(row.get("metric", "")).lower()
    if rust_value is not None and "ratio" in metric:
        return rust_value
    return None


def legacy_runtime_kind(record: dict) -> str:
    runtime = str(record.get("runtime", "")).strip().lower()
    runtime_kind = str(record.get("runtime_kind", "")).strip().lower()
    joined = " ".join(token for token in (runtime, runtime_kind) if token)
    if "bun" in joined:
        return "bun"
    if "node" in joined or runtime == "legacy_pi_mono":
        return "node"
    return "node"


legacy_cold_samples_ms_by_runtime: dict[str, list[float]] = {"node": [], "bun": []}
legacy_tool_samples_us_by_runtime: dict[str, list[float]] = {"node": [], "bun": []}
legacy_full_e2e_samples_ms_by_runtime: dict[str, list[float]] = {"node": [], "bun": []}
for record in legacy_records:
    runtime_kind = legacy_runtime_kind(record)
    scenario = str(record.get("scenario", "")).strip()
    if scenario == "ext_load_init/load_init_cold":
        summary = record.get("summary", {})
        if isinstance(summary, dict):
            p50_ms = parse_float(summary.get("p50_ms"))
            if p50_ms is not None:
                legacy_cold_samples_ms_by_runtime.setdefault(runtime_kind, []).append(p50_ms)
    if scenario == "ext_tool_call/hello":
        per_call_us = parse_float(record.get("per_call_us"))
        if per_call_us is not None:
            legacy_tool_samples_us_by_runtime.setdefault(runtime_kind, []).append(per_call_us)
    if scenario == "full_e2e_long_session":
        elapsed_ms = parse_float(record.get("elapsed_ms"))
        if elapsed_ms is not None:
            legacy_full_e2e_samples_ms_by_runtime.setdefault(runtime_kind, []).append(elapsed_ms)

legacy_cold_node_ms = mean(legacy_cold_samples_ms_by_runtime.get("node", []))
legacy_cold_bun_ms = mean(legacy_cold_samples_ms_by_runtime.get("bun", []))
legacy_tool_node_us = mean(legacy_tool_samples_us_by_runtime.get("node", []))
legacy_tool_bun_us = mean(legacy_tool_samples_us_by_runtime.get("bun", []))
legacy_full_e2e_node_ms = mean(legacy_full_e2e_samples_ms_by_runtime.get("node", []))
legacy_full_e2e_bun_ms = mean(legacy_full_e2e_samples_ms_by_runtime.get("bun", []))


def ratio_from_pair(rust_value, legacy_value):
    if rust_value is not None and legacy_value is not None and legacy_value > 0:
        return rust_value / legacy_value
    return None

cold_node_ratio = None
cold_bun_ratio = None
per_call_node_ratio = None
per_call_bun_ratio = None
full_e2e_node_ratio = None
full_e2e_bun_ratio = None
full_e2e_node_ratio_basis = "missing"

cold_ratio_row = comparison_row("rust-to-ts ratio", "load time")
if isinstance(cold_ratio_row, dict):
    cold_node_ratio = parse_float(cold_ratio_row.get("rust_value"))
if cold_node_ratio is None:
    cold_node_ratio = ratio_from_pair(cold_abs_ms, legacy_cold_node_ms)
cold_bun_ratio = ratio_from_pair(cold_abs_ms, legacy_cold_bun_ms)

per_call_row = comparison_row("hello per-call latency", "tool call")
if isinstance(per_call_row, dict):
    rust_value = parse_float(per_call_row.get("rust_value"))
    legacy_value = parse_float(per_call_row.get("legacy_value"))
    if rust_value is not None and legacy_value and legacy_value > 0:
        per_call_node_ratio = rust_value / legacy_value
if per_call_node_ratio is None:
    per_call_node_ratio = ratio_from_pair(per_call_abs_us, legacy_tool_node_us)
per_call_bun_ratio = ratio_from_pair(per_call_abs_us, legacy_tool_bun_us)

full_e2e_node_ratio = ratio_from_pair(full_e2e_abs_ms, legacy_full_e2e_node_ms)
if full_e2e_node_ratio is not None:
    full_e2e_node_ratio_basis = "node_legacy_extension_workloads"
if full_e2e_node_ratio is None:
    full_e2e_row = comparison_row("200 iters x 1 tool", "e2e process")
    full_e2e_node_ratio = extract_ratio_from_comparison_row(full_e2e_row)
    if full_e2e_node_ratio is not None:
        full_e2e_node_ratio_basis = "perf_comparison_full_e2e"
full_e2e_bun_ratio = ratio_from_pair(full_e2e_abs_ms, legacy_full_e2e_bun_ms)


def ratio_basis(value, measured_basis: str) -> str:
    if value is None:
        return "missing"
    return measured_basis


def build_layer(
    layer_id: str,
    display_name: str,
    scenario_tags: list[str],
    expected_suites: list[str],
    metric_name: str,
    absolute_value,
    absolute_unit: str,
    node_ratio,
    node_ratio_basis: str,
    bun_ratio,
    bun_ratio_basis: str,
    source_artifacts: list[Path],
    interpretation: str,
) -> dict:
    suite_statuses = {name: suite_status(name, suite_result_by_name) for name in expected_suites}
    absolute_present = absolute_value is not None
    relative_present = node_ratio is not None and bun_ratio is not None
    suites_with_data = [name for name, status in suite_statuses.items() if status != "missing"]
    all_ran_suites_passed = all(
        status == "pass" for status in suite_statuses.values() if status != "missing"
    )

    if absolute_present and relative_present and all_ran_suites_passed and suites_with_data:
        confidence = "high"
        evidence_state = "measured"
    elif absolute_present and (node_ratio is not None or bun_ratio is not None):
        confidence = "medium"
        evidence_state = "inferred"
    elif absolute_present:
        confidence = "low"
        evidence_state = "absolute_only"
    else:
        confidence = "low"
        evidence_state = "no_data"

    return {
        "layer_id": layer_id,
        "display_name": display_name,
        "scenario_tags": scenario_tags,
        "expected_suites": expected_suites,
        "suite_status": suite_statuses,
        "absolute_metrics": {
            "metric_name": metric_name,
            "value": absolute_value,
            "unit": absolute_unit,
        },
        "relative_metrics": {
            "rust_vs_node_ratio": node_ratio,
            "rust_vs_node_ratio_basis": node_ratio_basis,
            "rust_vs_bun_ratio": bun_ratio,
            "rust_vs_bun_ratio_basis": bun_ratio_basis,
        },
        "confidence": confidence,
        "evidence_state": evidence_state,
        "interpretation": interpretation,
        "lineage": {
            "run_id_lineage": [run_id, correlation_id],
            "source_artifacts": [str(path) for path in source_artifacts if path.exists()],
            "suite_logs": {suite: suite_log_paths(suite) for suite in expected_suites},
            "source_manifest_path": str(manifest_path),
        },
    }


layers = [
    build_layer(
        "cold_load_init",
        "Cold-load and initialization",
        ["cold-load", "init", "extension-runtime", "microbench"],
        ["ext_bench_harness", "bench_scenario"],
        "cold_load_p95",
        cold_abs_ms,
        "ms",
        cold_node_ratio,
        ratio_basis(cold_node_ratio, "node_legacy_extension_workloads"),
        cold_bun_ratio,
        ratio_basis(cold_bun_ratio, "bun_legacy_extension_workloads"),
        [
            ext_bench_path,
            ext_bench_report_path,
            scenario_runner_path,
            extension_bench_path,
        ],
        "Cold-load wins are attribution-only and must not be promoted as global UX claims.",
    ),
    build_layer(
        "per_call_dispatch_micro",
        "Per-call dispatch microbench",
        ["per-call", "dispatch", "hostcall", "microbench"],
        ["bench_scenario", "perf_bench_harness"],
        "dispatch_per_call",
        per_call_abs_us,
        "us",
        per_call_node_ratio,
        ratio_basis(per_call_node_ratio, "node_legacy_extension_workloads"),
        per_call_bun_ratio,
        ratio_basis(per_call_bun_ratio, "bun_legacy_extension_workloads"),
        [scenario_runner_path, extension_bench_path, workload_path],
        "Per-call improvements are diagnostic and cannot substitute for full-session outcomes.",
    ),
    build_layer(
        "full_e2e_long_session",
        "Full end-to-end long-session workload",
        ["full-e2e", "long-session", "user-facing", "release-facing"],
        ["perf_regression", "perf_comparison"],
        "long_session_elapsed",
        full_e2e_abs_ms,
        "ms",
        full_e2e_node_ratio,
        full_e2e_node_ratio_basis,
        full_e2e_bun_ratio,
        ratio_basis(full_e2e_bun_ratio, "bun_legacy_extension_workloads"),
        [workload_path, legacy_path, perf_comparison_path],
        "Full E2E evidence is the release-facing signal and must gate global speed claims.",
    ),
]

perf_sli = load_json(perf_sli_path) if perf_sli_path.exists() else {}
required_partition_tags = (
    perf_sli.get("reporting_contract", {}).get("required_partition_tags", [])
)
if not isinstance(required_partition_tags, list):
    required_partition_tags = []
required_partition_tags = [str(tag).strip() for tag in required_partition_tags if str(tag).strip()]
if not required_partition_tags:
    required_partition_tags = ["matched-state", "realistic"]

partition_coverage = {tag: False for tag in required_partition_tags}
if baseline_path.exists():
    baseline_payload = load_json(baseline_path)
    records = baseline_payload.get("records", [])
    if isinstance(records, list):
        for record in records:
            if not isinstance(record, dict):
                continue
            partition = str(record.get("workload_partition", "")).strip()
            if partition in partition_coverage:
                partition_coverage[partition] = True

layer_coverage = {
    layer["layer_id"]: (
        layer["absolute_metrics"]["value"] is not None
        and layer["relative_metrics"]["rust_vs_node_ratio"] is not None
        and layer["relative_metrics"]["rust_vs_bun_ratio"] is not None
    )
    for layer in layers
}

invalidity_reasons = []
for dataset in source_datasets:
    if dataset["required"] and dataset["accepted_record_count"] == 0:
        invalidity_reasons.append(f"missing_current_run_source:{dataset['path']}")
    if dataset["rejected_record_count"] > 0:
        invalidity_reasons.append(f"mixed_source_lineage:{dataset['path']}")
if not layer_coverage.get("full_e2e_long_session", False) and (
    layer_coverage.get("cold_load_init", False)
    or layer_coverage.get("per_call_dispatch_micro", False)
):
    invalidity_reasons.append("microbench_only_claim")

if not all(partition_coverage.values()):
    invalidity_reasons.append("global_claim_missing_partition_coverage")

for layer_id, covered in layer_coverage.items():
    if not covered:
        invalidity_reasons.append(f"missing_layer_coverage:{layer_id}")

global_claim_valid = len(invalidity_reasons) == 0

payload = {
    "schema": "pi.perf.extension_benchmark_stratification.v1",
    "bead_id": "bd-3ar8v.4.11",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "source_commit": source_commit,
    "source_dirty": source_dirty,
    "run_id": run_id,
    "correlation_id": correlation_id,
    "profile": str(manifest.get("profile", "unknown")),
    "execution_contract": {
        "orchestrator": "scripts/perf/orchestrate.sh",
        "layer_definition_version": "1.0.0",
        "required_layers": [
            "cold_load_init",
            "per_call_dispatch_micro",
            "full_e2e_long_session",
        ],
        "full_coverage_profiles": ["full", "ci"],
        "lineage_contract": "all layers must share run_id + correlation_id lineage",
    },
    "layers": layers,
    "source_datasets": source_datasets,
    "claim_integrity": {
        "anti_conflation": {
            "cold_load_wins_do_not_imply_per_call_or_e2e": True,
            "per_call_wins_do_not_imply_full_e2e": True,
            "full_e2e_is_release_facing_primary_signal": True,
        },
        "cherry_pick_guard": {
            "requires_all_layers_for_global_claim": True,
            "layer_coverage": layer_coverage,
            "global_claim_valid": global_claim_valid,
            "invalidity_reasons": sorted(set(invalidity_reasons)),
        },
        "required_partition_tags": required_partition_tags,
        "partition_coverage": partition_coverage,
        "policy_ref": "docs/perf_sli_matrix.json#ci_enforcement.fail_closed_conditions",
    },
    "lineage": {
        "run_id_lineage": [run_id, correlation_id],
        "source_manifest_path": str(manifest_path),
        "source_baseline_confidence_path": str(baseline_path) if baseline_path.exists() else None,
        "source_sli_contract_path": str(perf_sli_path) if perf_sli_path.exists() else None,
    },
}

stratification_path.parent.mkdir(parents=True, exist_ok=True)
stratification_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

manifest["extension_benchmark_stratification"] = {
    "schema": "pi.perf.extension_benchmark_stratification.v1",
    "path": str(stratification_path),
    "layer_count": len(layers),
    "global_claim_valid": global_claim_valid,
    "invalidity_reason_count": len(sorted(set(invalidity_reasons))),
}
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY
then
  artifact_count=$((artifact_count + 1))
  log_ok "Extension benchmark stratification written: results/extension_benchmark_stratification.json"
else
  die "Failed to generate extension benchmark stratification artifact"
fi

# ─── Phase 5e: Cross-environment variance diagnosis ─────────────────────────

log_phase "Phase 5e: Cross-Environment Variance Diagnosis"

if [[ -n "$CROSS_ENV_BASELINES" ]]; then
  CROSS_ENV_DIAG_PATH="$OUTPUT_DIR/results/cross_env_variance_diagnosis.json"
  IFS=';' read -r -a CROSS_ENV_ITEMS <<<"$CROSS_ENV_BASELINES"
  DIAG_ARGS=()
  for item in "${CROSS_ENV_ITEMS[@]}"; do
    item="${item#"${item%%[![:space:]]*}"}"
    item="${item%"${item##*[![:space:]]}"}"
    if [[ -n "$item" ]]; then
      DIAG_ARGS+=(--diagnose-env "$item")
    fi
  done

  if [[ "${#DIAG_ARGS[@]}" -lt 4 ]]; then
    die "PERF_CROSS_ENV_BASELINES must provide at least two label=path entries"
  fi

  log_step "Running cross-env diagnosis with ${#DIAG_ARGS[@]} parameters"
  if ./scripts/perf/capture_baseline.sh \
    "${DIAG_ARGS[@]}" \
    --diagnose-output "$CROSS_ENV_DIAG_PATH" \
    --variance-alert-pct "$CROSS_ENV_VARIANCE_ALERT_PCT"; then
    artifact_count=$((artifact_count + 1))
    log_ok "Cross-env diagnosis written: results/cross_env_variance_diagnosis.json"
  else
    die "Failed to generate cross-environment variance diagnosis artifact"
  fi

  CROSS_ENV_ALERT_COUNT="$(python3 - "$CROSS_ENV_DIAG_PATH" <<'PY'
import json, sys
payload = json.loads(open(sys.argv[1], encoding="utf-8").read())
print(int(payload.get("summary", {}).get("alert_count", 0)))
PY
)"
  CROSS_ENV_METRIC_COUNT="$(python3 - "$CROSS_ENV_DIAG_PATH" <<'PY'
import json, sys
payload = json.loads(open(sys.argv[1], encoding="utf-8").read())
print(int(payload.get("summary", {}).get("metric_count", 0)))
PY
)"

  if [[ "$CROSS_ENV_ENFORCE" == "1" && "${CROSS_ENV_ALERT_COUNT:-0}" -gt 0 ]]; then
    die "Cross-env diagnosis produced ${CROSS_ENV_ALERT_COUNT} alert(s) with PERF_CROSS_ENV_ENFORCE=1"
  fi

  if OUTPUT_DIR="$OUTPUT_DIR" CROSS_ENV_DIAG_PATH="$CROSS_ENV_DIAG_PATH" python3 - <<'PY'
import json
import os
from pathlib import Path

output_dir = Path(os.environ["OUTPUT_DIR"])
diag_path = Path(os.environ["CROSS_ENV_DIAG_PATH"])
manifest_path = output_dir / "manifest.json"
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
diag = json.loads(diag_path.read_text(encoding="utf-8"))
summary = diag.get("summary", {})
manifest["cross_env_variance_diagnosis"] = {
    "schema": "pi.perf.cross_env_variance_diagnosis.v1",
    "path": str(diag_path),
    "metric_count": int(summary.get("metric_count", 0)),
    "alert_count": int(summary.get("alert_count", 0)),
}
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY
  then
    :
  else
    die "Failed to record cross-env diagnosis metadata in manifest"
  fi
else
  log_step "Skipping cross-env diagnosis (set PERF_CROSS_ENV_BASELINES to enable)"
fi

# ─── Phase 5f: Phase-1 matrix validation ────────────────────────────────────

log_phase "Phase 5f: Phase-1 Matrix Validation"

PHASE1_MATRIX_PATH="$OUTPUT_DIR/results/phase1_matrix_validation.json"
PARAMETER_SWEEPS_PATH="$OUTPUT_DIR/results/parameter_sweeps.json"
OPPORTUNITY_MATRIX_PATH="$OUTPUT_DIR/results/opportunity_matrix.json"
if OUTPUT_DIR="$OUTPUT_DIR" \
  PROJECT_ROOT="$PROJECT_ROOT" \
  TARGET_DIR="$TARGET_DIR" \
  CORRELATION_ID="$CORRELATION_ID" \
  GIT_COMMIT_FULL="$GIT_COMMIT_FULL" \
  GIT_DIRTY="$GIT_DIRTY" \
  PI_PERF_STRICT="${PI_PERF_STRICT:-0}" \
  TIMESTAMP="$TIMESTAMP" \
  PHASE1_MATRIX_PATH="$PHASE1_MATRIX_PATH" \
  PARAMETER_SWEEPS_PATH="$PARAMETER_SWEEPS_PATH" \
  OPPORTUNITY_MATRIX_PATH="$OPPORTUNITY_MATRIX_PATH" \
  PERF_FAULT_INJECTION_ROOT="${PERF_FAULT_INJECTION_ROOT:-$PROJECT_ROOT/tests/e2e_results/persistence-fault-injection}" \
  python3 - <<'PY'
import hashlib
import json
import os
import re
from datetime import datetime, timedelta, timezone
from pathlib import Path

output_dir = Path(os.environ["OUTPUT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
target_dir = Path(os.environ["TARGET_DIR"])
correlation_id = os.environ["CORRELATION_ID"]
source_commit = os.environ["GIT_COMMIT_FULL"]
source_dirty = os.environ["GIT_DIRTY"] == "true"
strict_mode = os.environ["PI_PERF_STRICT"] == "1"
timestamp = os.environ["TIMESTAMP"]
run_started_at = datetime.strptime(timestamp, "%Y%m%dT%H%M%SZ").replace(
    tzinfo=timezone.utc
)
source_clock_skew = timedelta(seconds=120)
phase1_matrix_path = Path(os.environ["PHASE1_MATRIX_PATH"])
parameter_sweeps_path = Path(os.environ["PARAMETER_SWEEPS_PATH"])
opportunity_matrix_path = Path(os.environ["OPPORTUNITY_MATRIX_PATH"])

manifest_path = output_dir / "manifest.json"
scenario_runner_path = output_dir / "results" / "scenario_runner.jsonl"
scenario_runner_fallback_path = target_dir / "perf" / "scenario_runner.jsonl"
workload_path = output_dir / "results" / "pijs_workload.jsonl"
workload_fallback_path = target_dir / "perf" / "pijs_workload.jsonl"
stratification_path = output_dir / "results" / "extension_benchmark_stratification.json"
baseline_path = output_dir / "results" / "baseline_variance_confidence.json"
perf_sli_path = project_root / "docs" / "perf_sli_matrix.json"
fault_injection_script = project_root / "scripts" / "e2e" / "run_persistence_fault_injection.sh"
fault_injection_root = Path(os.environ["PERF_FAULT_INJECTION_ROOT"])


def load_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def load_jsonl(path: Path):
    if not path.exists():
        return []
    rows = []
    for line_number, line in enumerate(
        path.read_text(encoding="utf-8").splitlines(), start=1
    ):
        line = line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError as error:
            rows.append(
                {
                    "__lineage_parse_error": "invalid_json",
                    "line_number": line_number,
                    "detail": str(error),
                }
            )
            continue
        if isinstance(payload, dict):
            rows.append(payload)
        else:
            rows.append(
                {
                    "__lineage_parse_error": "non_object_json",
                    "line_number": line_number,
                }
            )
    return rows


def parse_record_timestamp(record):
    raw = record.get("timestamp", record.get("generated_at"))
    if not isinstance(raw, str) or not raw.strip():
        return None
    normalized = raw.strip()
    if normalized.endswith("Z"):
        normalized = f"{normalized[:-1]}+00:00"
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return None
    return parsed.astimezone(timezone.utc)


def parse_float(value):
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        value = value.strip()
        if not value:
            return None
        match = re.search(r"-?\d+(?:\.\d+)?", value)
        if match:
            return float(match.group(0))
    return None


def parse_int(value):
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    if isinstance(value, str):
        value = value.strip().replace("_", "")
        if not value:
            return None
        match = re.search(r"\d+", value)
        if match:
            return int(match.group(0))
    return None


def normalize_partition(value):
    text = str(value or "").strip().lower()
    text = text.replace("_", "-")
    if text in {"matched-state", "matchedstate"}:
        return "matched-state"
    if text == "realistic":
        return "realistic"
    return text


def parse_session_size(scenario_id, replay_input):
    if isinstance(replay_input, dict):
        direct = parse_int(replay_input.get("session_messages"))
        if direct is not None:
            return direct
    if not scenario_id:
        return None
    match = re.search(r"session[_/-]?(\d+)", scenario_id)
    if match:
        return int(match.group(1))
    return None


def suite_status(name, suite_map):
    row = suite_map.get(name)
    if not isinstance(row, dict):
        return "missing"
    status = str(row.get("status", "")).strip().lower()
    return status if status else "missing"


manifest = load_json(manifest_path)
run_id = str(manifest.get("timestamp", timestamp))

suite_results = manifest.get("suite_results", [])
if not isinstance(suite_results, list):
    suite_results = []
suite_result_by_name = {
    str(row.get("suite", "")).strip(): row
    for row in suite_results
    if isinstance(row, dict) and str(row.get("suite", "")).strip()
}

perf_sli = load_json(perf_sli_path) if perf_sli_path.exists() else {}
required_partitions = (
    perf_sli.get("reporting_contract", {}).get("required_partition_tags", [])
)
if not isinstance(required_partitions, list):
    required_partitions = []
required_partitions = [
    normalize_partition(tag) for tag in required_partitions if normalize_partition(tag)
]
if not required_partitions:
    required_partitions = ["matched-state", "realistic"]

benchmark_partitions = perf_sli.get("benchmark_partitions", {})
required_sizes = []
if isinstance(benchmark_partitions, dict):
    realistic_ids = benchmark_partitions.get("realistic_long_session", [])
    if isinstance(realistic_ids, list):
        for item in realistic_ids:
            parsed = parse_session_size(str(item), {})
            if parsed is not None and parsed not in required_sizes:
                required_sizes.append(parsed)
if not required_sizes:
    required_sizes = [100_000, 200_000, 500_000, 1_000_000, 5_000_000]

effective_scenario_runner_path = scenario_runner_path
scenario_runner_records = load_jsonl(scenario_runner_path)
if not scenario_runner_records and not strict_mode and scenario_runner_fallback_path.exists():
    scenario_runner_records = load_jsonl(scenario_runner_fallback_path)
    effective_scenario_runner_path = scenario_runner_fallback_path

effective_workload_path = workload_path
workload_records = load_jsonl(workload_path)
if not workload_records and not strict_mode and workload_fallback_path.exists():
    workload_records = load_jsonl(workload_fallback_path)
    effective_workload_path = workload_fallback_path


def admit_dataset(path, records, correlation_field):
    accepted = []
    rejected = []
    for index, record in enumerate(records):
        reasons = []
        observed_timestamp = parse_record_timestamp(record)
        if record.get("__lineage_parse_error"):
            reasons.append(str(record["__lineage_parse_error"]))
        if record.get(correlation_field) != correlation_id:
            reasons.append("correlation_id_mismatch")
        if record.get("run_id") != correlation_id:
            reasons.append("run_id_mismatch")
        if record.get("source_commit") != source_commit:
            reasons.append("source_commit_mismatch")
        if record.get("source_dirty") is not False:
            reasons.append("source_dirty_not_false")
        if observed_timestamp is None:
            reasons.append("missing_or_invalid_timestamp")
        elif observed_timestamp < run_started_at - source_clock_skew:
            reasons.append("timestamp_before_run_start")
        elif observed_timestamp > datetime.now(timezone.utc) + source_clock_skew:
            reasons.append("timestamp_in_future")
        if reasons:
            rejected.append({"record_index": index, "reasons": reasons})
        else:
            accepted.append(record)
    digest = hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None
    return accepted, {
        "path": str(path),
        "sha256": digest,
        "correlation_field": correlation_field,
        "expected_correlation_id": correlation_id,
        "expected_source_commit": source_commit,
        "expected_min_timestamp": (
            run_started_at - source_clock_skew
        ).isoformat().replace("+00:00", "Z"),
        "accepted_record_count": len(accepted),
        "rejected_record_count": len(rejected),
        "rejections": rejected,
    }


scenario_runner_records, scenario_dataset = admit_dataset(
    effective_scenario_runner_path,
    scenario_runner_records,
    "orchestration_correlation_id",
)
workload_records, workload_dataset = admit_dataset(
    effective_workload_path,
    workload_records,
    "correlation_id",
)
source_datasets = [scenario_dataset, workload_dataset]
source_lineage_valid = all(
    dataset["accepted_record_count"] > 0 and dataset["rejected_record_count"] == 0
    for dataset in source_datasets
)


def parse_partition(record, metadata, scenario_id):
    partition = normalize_partition(
        record.get("partition")
        or record.get("workload_partition")
        or metadata.get("partition")
        or metadata.get("workload_partition")
    )
    if partition in {"matched-state", "realistic"}:
        return partition
    scenario_norm = normalize_partition(record.get("scenario"))
    if scenario_norm in {"matched-state", "realistic"}:
        return scenario_norm
    if scenario_id.startswith("matched-state/"):
        return "matched-state"
    if scenario_id.startswith("realistic/"):
        return "realistic"
    return partition


stage_records = {}
for source_name, records in (
    ("scenario_runner", scenario_runner_records),
    ("pijs_workload", workload_records),
):
    for index, record in enumerate(records):
        if not isinstance(record, dict):
            continue

        metadata = record.get("scenario_metadata")
        if not isinstance(metadata, dict):
            metadata = {}
        replay_input = metadata.get("replay_input")
        if not isinstance(replay_input, dict):
            replay_input = {}

        scenario_id = str(
            metadata.get("scenario_id")
            or record.get("scenario_id")
            or record.get("scenario")
            or ""
        ).strip()
        partition = parse_partition(record, metadata, scenario_id)
        session_messages = parse_session_size(scenario_id, replay_input)
        if session_messages is None:
            session_messages = parse_int(
                record.get("session_messages")
                or record.get("message_count")
                or replay_input.get("session_messages")
            )

        if partition not in required_partitions or session_messages not in required_sizes:
            continue

        stage_attribution = record.get("stage_attribution")
        if not isinstance(stage_attribution, dict):
            stage_attribution = {}

        open_ms = parse_float(record.get("open_ms"))
        if open_ms is None:
            open_ms = parse_float(stage_attribution.get("open_ms"))
        append_ms = parse_float(record.get("append_ms"))
        if append_ms is None:
            append_ms = parse_float(stage_attribution.get("append_ms"))
        save_ms = parse_float(record.get("save_ms"))
        if save_ms is None:
            save_ms = parse_float(stage_attribution.get("save_ms"))
        index_ms = parse_float(record.get("index_ms"))
        if index_ms is None:
            index_ms = parse_float(record.get("session_index_ms"))
        if index_ms is None:
            index_ms = parse_float(stage_attribution.get("index_ms"))
        wall_clock_ms = parse_float(record.get("total_ms"))
        if wall_clock_ms is None:
            wall_clock_ms = parse_float(record.get("elapsed_ms"))
        if wall_clock_ms is None:
            wall_clock_ms = parse_float(stage_attribution.get("total_stage_ms"))
        source_swarm_metrics = record.get("swarm_metrics")
        if not isinstance(source_swarm_metrics, dict):
            source_swarm_metrics = None

        candidate = {
            "scenario_id": scenario_id
            if scenario_id
            else f"{partition}/session_{session_messages}",
            "open_ms": open_ms,
            "append_ms": append_ms,
            "save_ms": save_ms,
            "index_ms": index_ms,
            "wall_clock_ms": wall_clock_ms,
            "swarm_metrics": source_swarm_metrics,
            "source_record_index": index,
            "source_name": source_name,
        }

        key = (partition, session_messages)
        if key in stage_records:
            existing = stage_records[key]
            existing_score = sum(
                1
                for metric in ("open_ms", "append_ms", "save_ms", "index_ms", "wall_clock_ms")
                if existing.get(metric) is not None
            )
            candidate_score = sum(
                1
                for metric in ("open_ms", "append_ms", "save_ms", "index_ms", "wall_clock_ms")
                if candidate.get(metric) is not None
            )
            if existing_score >= candidate_score:
                continue

        stage_records[key] = candidate

stratification = load_json(stratification_path) if stratification_path.exists() else {}
layers = stratification.get("layers", [])
if not isinstance(layers, list):
    layers = []
layer_by_id = {
    str(layer.get("layer_id", "")).strip(): layer
    for layer in layers
    if isinstance(layer, dict) and str(layer.get("layer_id", "")).strip()
}

def layer_absolute(layer_id):
    layer = layer_by_id.get(layer_id, {})
    if not isinstance(layer, dict):
        return None
    metrics = layer.get("absolute_metrics", {})
    if not isinstance(metrics, dict):
        return None
    return parse_float(metrics.get("value"))


def layer_relative(layer_id, field):
    layer = layer_by_id.get(layer_id, {})
    if not isinstance(layer, dict):
        return None
    metrics = layer.get("relative_metrics", {})
    if not isinstance(metrics, dict):
        return None
    return parse_float(metrics.get(field))


primary_wall_clock_ms = layer_absolute("full_e2e_long_session")
primary_rust_vs_node_ratio = layer_relative(
    "full_e2e_long_session", "rust_vs_node_ratio"
)
primary_rust_vs_bun_ratio = layer_relative("full_e2e_long_session", "rust_vs_bun_ratio")
cold_load_ms = layer_absolute("cold_load_init")
per_call_us = layer_absolute("per_call_dispatch_micro")

cells = []
required_stage_keys = ["open_ms", "append_ms", "save_ms", "index_ms"]
required_latency_quantiles = ["p50", "p95", "p99", "p999"]
required_queue_depth_quantiles = ["p50", "p95", "p99", "p999", "max"]
required_resource_usage_keys = ["rss_mb", "cpu_pct"]
required_component_breakdown_keys = ["tool", "provider", "extension", "session"]
required_stage_breakdown_keys = ["open", "append", "save", "index"]
required_host_capacity_keys = ["target_cpu_cores", "observed_cpu_cores", "mem_total_mb"]
operation_stage_coverage = {
    "open_ms": 0,
    "append_ms": 0,
    "save_ms": 0,
    "index_ms": 0,
}
covered_cells = 0
cells_with_complete_stage_breakdown = 0
cells_with_complete_swarm_metrics = 0


def normalize_metric_group(source: dict | None, keys: list[str]) -> tuple[dict, list[str]]:
    normalized = {}
    missing = []
    if not isinstance(source, dict):
        for key in keys:
            normalized[key] = None
            missing.append(key)
        return normalized, missing

    for key in keys:
        value = parse_float(source.get(key))
        if value is None or value < 0:
            normalized[key] = None
            missing.append(key)
        else:
            normalized[key] = value

    return normalized, missing


def normalize_swarm_metrics(source: dict | None) -> tuple[dict, list[str]]:
    groups = [
        ("latency_quantiles_ms", required_latency_quantiles),
        ("queue_depth", required_queue_depth_quantiles),
        ("resource_usage", required_resource_usage_keys),
        ("component_breakdown_ms", required_component_breakdown_keys),
        ("stage_breakdown_ms", required_stage_breakdown_keys),
        ("host_capacity", required_host_capacity_keys),
    ]
    normalized = {}
    missing = []
    for group_name, required_keys in groups:
        group_source = source.get(group_name) if isinstance(source, dict) else None
        group, group_missing = normalize_metric_group(group_source, required_keys)
        normalized[group_name] = group
        missing.extend(f"{group_name}.{key}" for key in group_missing)

    return normalized, missing

for partition in required_partitions:
    for session_messages in required_sizes:
        key = (partition, session_messages)
        source = stage_records.get(key, {})

        stage_attribution = {
            "open_ms": source.get("open_ms"),
            "append_ms": source.get("append_ms"),
            "save_ms": source.get("save_ms"),
            "index_ms": source.get("index_ms"),
        }
        for metric, value in stage_attribution.items():
            if value is not None:
                operation_stage_coverage[metric] += 1

        missing_stage_keys = [
            metric for metric in required_stage_keys if stage_attribution.get(metric) is None
        ]
        if not missing_stage_keys:
            cells_with_complete_stage_breakdown += 1

        total_stage_ms = sum(
            value for value in stage_attribution.values() if value is not None
        )
        if all(value is None for value in stage_attribution.values()):
            total_stage_ms = None

        cell_wall_clock = source.get("wall_clock_ms")
        if cell_wall_clock is None:
            cell_wall_clock = primary_wall_clock_ms

        missing_reasons = []
        if not source:
            missing_reasons.append("missing_matrix_source_record")
        if missing_stage_keys:
            missing_reasons.append(
                "missing_stage_metrics:" + ",".join(sorted(missing_stage_keys))
            )
        if cell_wall_clock is None:
            missing_reasons.append("missing_primary_wall_clock")
        if primary_rust_vs_node_ratio is None or primary_rust_vs_bun_ratio is None:
            missing_reasons.append("missing_primary_relative_ratios")

        if source:
            covered_cells += 1

        swarm_metrics, missing_swarm_keys = normalize_swarm_metrics(
            source.get("swarm_metrics") if source else None
        )
        if missing_swarm_keys:
            missing_reasons.append(
                "missing_swarm_metrics:" + ",".join(sorted(missing_swarm_keys))
            )
        else:
            cells_with_complete_swarm_metrics += 1

        cells.append(
            {
                "workload_partition": partition,
                "session_messages": session_messages,
                "scenario_id": source.get("scenario_id")
                or f"{partition}/session_{session_messages}",
                "status": "pass" if not missing_reasons else "fail",
                "missing_reasons": sorted(set(missing_reasons)),
                "stage_attribution": {
                    **stage_attribution,
                    "total_stage_ms": total_stage_ms,
                },
                "swarm_metrics": swarm_metrics,
                "primary_e2e": {
                    "wall_clock_ms": cell_wall_clock,
                    "rust_vs_node_ratio": primary_rust_vs_node_ratio,
                    "rust_vs_bun_ratio": primary_rust_vs_bun_ratio,
                },
                "microbench_context": {
                    "cold_load_ms": cold_load_ms,
                    "per_call_us": per_call_us,
                },
                "lineage": {
                    "source_record_index": source.get("source_record_index"),
                    "source_record_stream": source.get("source_name"),
                    "source_artifacts": [
                        str(path)
                        for path in (
                            effective_scenario_runner_path,
                            effective_workload_path,
                            stratification_path,
                            baseline_path,
                        )
                        if path.exists()
                    ],
                },
            }
        )

missing_cells = [
    {
        "workload_partition": cell["workload_partition"],
        "session_messages": cell["session_messages"],
        "reasons": cell["missing_reasons"],
    }
    for cell in cells
    if any(
        isinstance(reason, str) and reason.startswith("missing_stage_metrics:")
        for reason in cell.get("missing_reasons", [])
    )
]
swarm_missing_cells = [
    {
        "workload_partition": cell["workload_partition"],
        "session_messages": cell["session_messages"],
        "reasons": cell["missing_reasons"],
    }
    for cell in cells
    if any(
        isinstance(reason, str) and reason.startswith("missing_swarm_metrics")
        for reason in cell.get("missing_reasons", [])
    )
]

def compute_weighted_bottleneck_attribution(
    matrix_cells: list[dict],
    stage_keys: list[str],
    required_scales: list[int],
    required_partition_tags: list[str],
) -> dict:
    valid_cells: list[dict] = []
    for cell in matrix_cells:
        if not isinstance(cell, dict):
            continue
        if str(cell.get("status", "")).strip().lower() != "pass":
            continue
        stage_attribution = cell.get("stage_attribution")
        if not isinstance(stage_attribution, dict):
            continue
        total_stage_ms = parse_float(stage_attribution.get("total_stage_ms"))
        if total_stage_ms is None or total_stage_ms <= 0:
            continue
        valid_cells.append(cell)

    if not valid_cells:
        return {
            "schema": "pi.perf.phase1_weighted_bottleneck_attribution.v1",
            "status": "missing",
            "weighting_policy": "session_messages",
            "confidence_method": "weighted_normal_approx_95",
            "reason": "no_pass_cells_with_stage_totals",
            "per_scale": [],
            "global_ranking": [],
            "lineage": {
                "source_stream": "phase1_matrix_validation.matrix_cells",
                "source_cell_count": len(matrix_cells),
                "valid_cell_count": 0,
            },
        }

    per_scale = []
    for session_messages in required_scales:
        partitions = []
        for partition in required_partition_tags:
            selected = next(
                (
                    cell
                    for cell in valid_cells
                    if str(cell.get("workload_partition", "")).strip() == partition
                    and parse_int(cell.get("session_messages")) == session_messages
                ),
                None,
            )
            if not selected:
                partitions.append(
                    {
                        "workload_partition": partition,
                        "present": False,
                        "scenario_id": f"{partition}/session_{session_messages}",
                        "stage_pct": {stage: None for stage in stage_keys},
                    }
                )
                continue

            stage_attribution = selected.get("stage_attribution", {})
            total_stage_ms = parse_float(stage_attribution.get("total_stage_ms"))
            if not isinstance(stage_attribution, dict) or total_stage_ms is None or total_stage_ms <= 0:
                partitions.append(
                    {
                        "workload_partition": partition,
                        "present": True,
                        "scenario_id": selected.get("scenario_id"),
                        "stage_pct": {stage: None for stage in stage_keys},
                    }
                )
                continue

            stage_pct = {}
            for stage in stage_keys:
                stage_value = parse_float(stage_attribution.get(stage))
                stage_pct[stage] = (
                    (stage_value / total_stage_ms) * 100.0
                    if stage_value is not None and stage_value >= 0
                    else None
                )

            partitions.append(
                {
                    "workload_partition": partition,
                    "present": True,
                    "scenario_id": selected.get("scenario_id"),
                    "total_stage_ms": total_stage_ms,
                    "stage_pct": stage_pct,
                }
            )

        per_scale.append(
            {
                "session_messages": session_messages,
                "partitions": partitions,
            }
        )

    weighted_stage_ms = {stage: 0.0 for stage in stage_keys}
    weighted_total_stage_ms = 0.0
    stage_share_observations: dict[str, list[tuple[float, float]]] = {
        stage: [] for stage in stage_keys
    }

    for cell in valid_cells:
        stage_attribution = cell.get("stage_attribution", {})
        if not isinstance(stage_attribution, dict):
            continue
        total_stage_ms = parse_float(stage_attribution.get("total_stage_ms"))
        if total_stage_ms is None or total_stage_ms <= 0:
            continue
        session_messages = parse_int(cell.get("session_messages"))
        cell_weight = float(session_messages if session_messages and session_messages > 0 else 1)
        weighted_total_stage_ms += total_stage_ms * cell_weight
        for stage in stage_keys:
            stage_value = parse_float(stage_attribution.get(stage))
            if stage_value is None:
                continue
            weighted_stage_ms[stage] += stage_value * cell_weight
            stage_share_observations[stage].append((stage_value / total_stage_ms, cell_weight))

    def weighted_confidence_interval(observations: list[tuple[float, float]]):
        if not observations:
            return (None, None, None)
        total_weight = sum(weight for _, weight in observations)
        if total_weight <= 0:
            return (None, None, None)
        mean_share = sum(share * weight for share, weight in observations) / total_weight
        total_weight_sq = sum(weight * weight for _, weight in observations)
        if total_weight_sq <= 0:
            return (mean_share, None, None)
        effective_n = (total_weight * total_weight) / total_weight_sq
        variance = (
            sum(weight * ((share - mean_share) ** 2) for share, weight in observations)
            / total_weight
        )
        if effective_n <= 1:
            return (mean_share, None, None)
        standard_error = (variance / effective_n) ** 0.5
        delta = 1.96 * standard_error
        lower = max(0.0, mean_share - delta)
        upper = min(1.0, mean_share + delta)
        return (mean_share, lower, upper)

    global_ranking = []
    for stage in stage_keys:
        weighted_ms = weighted_stage_ms[stage]
        contribution_pct = (
            (weighted_ms / weighted_total_stage_ms) * 100.0
            if weighted_total_stage_ms > 0
            else None
        )
        mean_share, ci95_lower, ci95_upper = weighted_confidence_interval(
            stage_share_observations[stage]
        )
        global_ranking.append(
            {
                "stage": stage,
                "weighted_stage_ms": weighted_ms,
                "weighted_contribution_pct": contribution_pct,
                "mean_share_pct": (mean_share * 100.0) if mean_share is not None else None,
                "ci95_lower_pct": (ci95_lower * 100.0) if ci95_lower is not None else None,
                "ci95_upper_pct": (ci95_upper * 100.0) if ci95_upper is not None else None,
                "sample_size": len(stage_share_observations[stage]),
            }
        )

    global_ranking.sort(
        key=lambda row: row.get("weighted_contribution_pct") or -1.0,
        reverse=True,
    )

    return {
        "schema": "pi.perf.phase1_weighted_bottleneck_attribution.v1",
        "status": "computed",
        "weighting_policy": "session_messages",
        "confidence_method": "weighted_normal_approx_95",
        "per_scale": per_scale,
        "global_ranking": global_ranking,
        "lineage": {
            "source_stream": "phase1_matrix_validation.matrix_cells",
            "source_cell_count": len(matrix_cells),
            "valid_cell_count": len(valid_cells),
        },
    }


weighted_bottleneck_attribution = compute_weighted_bottleneck_attribution(
    cells,
    required_stage_keys,
    required_sizes,
    required_partitions,
)


def compute_parameter_sweep_sensitivity(
    per_scale_rows: list[dict],
    stage_order: list[str],
) -> tuple[list[dict], list[dict]]:
    stage_values: dict[str, list[float]] = {stage: [] for stage in stage_order}
    per_scale_summary: list[dict] = []

    for row in per_scale_rows:
        if not isinstance(row, dict):
            continue

        session_messages = parse_int(row.get("session_messages"))
        partitions = row.get("partitions", [])
        if not isinstance(partitions, list):
            partitions = []

        per_stage_ranges = {}
        for stage in stage_order:
            observed = []
            for partition in partitions:
                if not isinstance(partition, dict):
                    continue
                stage_pct = partition.get("stage_pct")
                if not isinstance(stage_pct, dict):
                    continue
                value = parse_float(stage_pct.get(stage))
                if value is None:
                    continue
                observed.append(value)
                stage_values[stage].append(value)

            if observed:
                min_pct = min(observed)
                max_pct = max(observed)
                per_stage_ranges[stage] = {
                    "min_pct": min_pct,
                    "max_pct": max_pct,
                    "spread_pct": max_pct - min_pct,
                    "sample_size": len(observed),
                }
            else:
                per_stage_ranges[stage] = {
                    "min_pct": None,
                    "max_pct": None,
                    "spread_pct": None,
                    "sample_size": 0,
                }

        per_scale_summary.append(
            {
                "session_messages": session_messages,
                "stage_ranges": per_stage_ranges,
            }
        )

    global_stage_spread: list[dict] = []
    for stage in stage_order:
        values = stage_values.get(stage, [])
        if values:
            min_pct = min(values)
            max_pct = max(values)
            global_stage_spread.append(
                {
                    "stage": stage,
                    "min_pct": min_pct,
                    "max_pct": max_pct,
                    "spread_pct": max_pct - min_pct,
                    "sample_size": len(values),
                }
            )
        else:
            global_stage_spread.append(
                {
                    "stage": stage,
                    "min_pct": None,
                    "max_pct": None,
                    "spread_pct": None,
                    "sample_size": 0,
                }
            )

    return per_scale_summary, global_stage_spread


def compute_parameter_sweeps_artifact(
    weighted: dict,
    required_scales: list[int],
    required_partition_tags: list[str],
    readiness_gate_passed: bool,
    source_artifact_path: Path,
) -> dict:
    weighted_status = str(weighted.get("status", "missing")).strip().lower()
    weighted_schema = str(weighted.get("schema", "")).strip()

    global_ranking = weighted.get("global_ranking")
    if not isinstance(global_ranking, list):
        global_ranking = []
    per_scale = weighted.get("per_scale")
    if not isinstance(per_scale, list):
        per_scale = []

    stage_order = []
    for row in global_ranking:
        if not isinstance(row, dict):
            continue
        stage = str(row.get("stage", "")).strip()
        if stage and stage not in stage_order:
            stage_order.append(stage)
    if not stage_order:
        stage_order = list(required_stage_keys)

    top_stage = stage_order[0] if stage_order else "save_ms"

    defaults_by_stage = {
        "open_ms": {
            "flush_cadence_ms": 1000,
            "queue_max_items": 2048,
            "compaction_quota_mb": 128,
        },
        "append_ms": {
            "flush_cadence_ms": 750,
            "queue_max_items": 4096,
            "compaction_quota_mb": 128,
        },
        "save_ms": {
            "flush_cadence_ms": 500,
            "queue_max_items": 3072,
            "compaction_quota_mb": 96,
        },
        "index_ms": {
            "flush_cadence_ms": 1250,
            "queue_max_items": 1536,
            "compaction_quota_mb": 192,
        },
    }
    selected_defaults = defaults_by_stage.get(top_stage, defaults_by_stage["save_ms"])

    per_scale_summary, global_stage_spread = compute_parameter_sweep_sensitivity(
        per_scale, stage_order
    )

    blocking_reasons = []
    if weighted_status != "computed":
        blocking_reasons.append("weighted_bottleneck_attribution_not_computed")
    if not per_scale_summary:
        blocking_reasons.append("missing_per_scale_sensitivity_inputs")
    if not readiness_gate_passed:
        blocking_reasons.append("phase1_matrix_not_ready_for_phase5")

    top_stage_spread = next(
        (
            row.get("spread_pct")
            for row in global_stage_spread
            if isinstance(row, dict) and row.get("stage") == top_stage
        ),
        None,
    )
    stability = "insufficient_data"
    if isinstance(top_stage_spread, (int, float)):
        stability = "stable" if top_stage_spread <= 20.0 else "unstable"

    readiness_ok = not blocking_reasons
    readiness_status = "ready" if readiness_ok else "blocked"

    return {
        "schema": "pi.perf.parameter_sweeps.v1",
        "bead_id": "bd-3ar8v.6.2",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "run_id": run_id,
        "correlation_id": correlation_id,
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": str(source_artifact_path),
            "weighted_bottleneck_schema": weighted_schema,
            "weighted_bottleneck_status": weighted_status,
        },
        "objective": {
            "primary_metric": "full_e2e_long_session.wall_clock_ms",
            "secondary_metrics": [
                "open_ms",
                "append_ms",
                "save_ms",
                "index_ms",
                "rust_vs_node_ratio",
                "rust_vs_bun_ratio",
            ],
            "constraints": [
                "memory_regression_guard=pass",
                "correctness_regression_guard=pass",
                "security_regression_guard=pass",
            ],
        },
        "sweep_plan": {
            "workload_partition_tags": required_partition_tags,
            "session_message_sizes": required_scales,
            "dimensions": [
                {
                    "name": "flush_cadence_ms",
                    "candidate_values": [250, 500, 750, 1000, 1500, 2000],
                },
                {
                    "name": "queue_max_items",
                    "candidate_values": [512, 1024, 2048, 3072, 4096, 8192],
                },
                {
                    "name": "compaction_quota_mb",
                    "candidate_values": [32, 64, 96, 128, 192, 256],
                },
            ],
            "analysis_method": "weighted_bottleneck_guided_grid",
            "outlier_policy": "fail_closed_on_unstable_or_missing_weighted_inputs",
        },
        "selected_defaults": {
            "flush_cadence_ms": selected_defaults["flush_cadence_ms"],
            "queue_max_items": selected_defaults["queue_max_items"],
            "compaction_quota_mb": selected_defaults["compaction_quota_mb"],
            "selection_basis": f"top_stage={top_stage}",
        },
        "sensitivity_summary": {
            "top_stage": top_stage,
            "stability": stability,
            "per_scale_stage_ranges": per_scale_summary,
            "global_stage_spread": global_stage_spread,
        },
        "readiness": {
            "status": readiness_status,
            "ready_for_phase5": readiness_ok,
            "blocking_reasons": blocking_reasons,
            "fail_closed_conditions": [
                "weighted_bottleneck_attribution_not_computed",
                "missing_per_scale_sensitivity_inputs",
                "phase1_matrix_not_ready_for_phase5",
            ],
        },
        "lineage": {
            "source_stream": "phase1_matrix_validation.weighted_bottleneck_attribution",
            "source_run_id": run_id,
            "source_correlation_id": correlation_id,
        },
    }


def compute_opportunity_matrix_artifact(
    weighted: dict,
    readiness_gate_passed: bool,
    source_artifact_path: Path,
) -> dict:
    weighted_status = str(weighted.get("status", "missing")).strip().lower()
    weighted_schema = str(weighted.get("schema", "")).strip()
    weighted_lineage = weighted.get("lineage", {})
    if not isinstance(weighted_lineage, dict):
        weighted_lineage = {}

    global_ranking = weighted.get("global_ranking")
    if not isinstance(global_ranking, list):
        global_ranking = []

    blocking_reasons = []
    if weighted_status != "computed":
        blocking_reasons.append("weighted_bottleneck_attribution_not_computed")
    if not readiness_gate_passed:
        blocking_reasons.append("phase1_matrix_not_ready_for_phase5")
    if not global_ranking:
        blocking_reasons.append("missing_weighted_global_ranking")
    if not run_id or not correlation_id:
        blocking_reasons.append("insufficient_freshness_identity")

    source_stream = str(weighted_lineage.get("source_stream", "")).strip()
    source_cell_count = parse_int(weighted_lineage.get("source_cell_count"))
    valid_cell_count = parse_int(weighted_lineage.get("valid_cell_count"))
    if source_stream != "phase1_matrix_validation.matrix_cells":
        blocking_reasons.append("lineage_source_stream_mismatch")
    if source_cell_count is None or source_cell_count <= 0:
        blocking_reasons.append("missing_lineage_source_cell_count")
    if valid_cell_count is None or valid_cell_count <= 0:
        blocking_reasons.append("missing_lineage_valid_cell_count")
    elif source_cell_count is not None and valid_cell_count > source_cell_count:
        blocking_reasons.append("lineage_valid_cell_count_exceeds_source_count")

    effort_profile = {
        "open_ms": {"points": 2.5, "level": "medium"},
        "append_ms": {"points": 3.0, "level": "high"},
        "save_ms": {"points": 3.0, "level": "high"},
        "index_ms": {"points": 2.0, "level": "medium"},
    }
    user_impact_profile = {
        "open_ms": {
            "resume_latency": "high",
            "extension_responsiveness": "low",
            "failure_risk": "medium",
        },
        "append_ms": {
            "resume_latency": "high",
            "extension_responsiveness": "medium",
            "failure_risk": "high",
        },
        "save_ms": {
            "resume_latency": "medium",
            "extension_responsiveness": "low",
            "failure_risk": "high",
        },
        "index_ms": {
            "resume_latency": "high",
            "extension_responsiveness": "medium",
            "failure_risk": "medium",
        },
    }

    ranked_candidates = []
    for row in global_ranking:
        if not isinstance(row, dict):
            continue
        stage = str(row.get("stage", "")).strip()
        if not stage:
            continue

        weighted_contribution_pct = parse_float(row.get("weighted_contribution_pct"))
        sample_size = parse_int(row.get("sample_size"))
        ci95_lower_pct = parse_float(row.get("ci95_lower_pct"))
        ci95_upper_pct = parse_float(row.get("ci95_upper_pct"))

        if weighted_contribution_pct is None or weighted_contribution_pct < 0:
            continue

        ci95_width_pct = None
        if (
            ci95_lower_pct is not None
            and ci95_upper_pct is not None
            and ci95_upper_pct >= ci95_lower_pct
        ):
            ci95_width_pct = ci95_upper_pct - ci95_lower_pct

        confidence_level = "low"
        confidence_score = 0.25
        if (
            sample_size is not None
            and sample_size >= 2
            and isinstance(ci95_width_pct, (int, float))
        ):
            if ci95_width_pct <= 15.0:
                confidence_level = "high"
                confidence_score = 0.90
            elif ci95_width_pct <= 30.0:
                confidence_level = "medium"
                confidence_score = 0.60
            else:
                confidence_level = "low"
                confidence_score = 0.35
        elif sample_size is not None and sample_size >= 3:
            confidence_level = "medium"
            confidence_score = 0.50

        sufficient_for_decision = (
            sample_size is not None
            and sample_size >= 2
            and isinstance(ci95_width_pct, (int, float))
            and ci95_width_pct <= 30.0
        )

        effort = effort_profile.get(stage, {"points": 2.5, "level": "medium"})
        effort_points = parse_float(effort.get("points")) or 2.5
        expected_gain_pct = weighted_contribution_pct * 0.85
        priority_score = (expected_gain_pct * confidence_score) / effort_points

        rationale = []
        if stage == "open_ms":
            rationale.append("resume/open path dominates long-session user wait time")
        elif stage == "append_ms":
            rationale.append("append throughput dominates sustained session mutation cost")
        elif stage == "save_ms":
            rationale.append("save path controls durability/latency tradeoff under load")
        elif stage == "index_ms":
            rationale.append("index rebuild/query overhead impacts resume responsiveness")
        rationale.append("weighted E2E contribution derived from phase1 matrix attribution")

        ranked_candidates.append(
            {
                "stage": stage,
                "weighted_contribution_pct": weighted_contribution_pct,
                "expected_gain_pct": expected_gain_pct,
                "priority_score": priority_score,
                "confidence": {
                    "level": confidence_level,
                    "score": confidence_score,
                    "sample_size": sample_size,
                    "ci95_lower_pct": ci95_lower_pct,
                    "ci95_upper_pct": ci95_upper_pct,
                    "ci95_width_pct": ci95_width_pct,
                    "sufficient_for_decision": sufficient_for_decision,
                },
                "effort": {
                    "points": effort_points,
                    "level": str(effort.get("level", "medium")),
                },
                "user_impact": user_impact_profile.get(
                    stage,
                    {
                        "resume_latency": "medium",
                        "extension_responsiveness": "medium",
                        "failure_risk": "medium",
                    },
                ),
                "rationale": rationale,
            }
        )

    if not ranked_candidates:
        blocking_reasons.append("missing_rankable_opportunities")

    ranked_candidates.sort(
        key=lambda row: (
            row.get("priority_score") if isinstance(row.get("priority_score"), (int, float)) else -1.0,
            row.get("weighted_contribution_pct")
            if isinstance(row.get("weighted_contribution_pct"), (int, float))
            else -1.0,
        ),
        reverse=True,
    )

    if ranked_candidates:
        top_confidence = ranked_candidates[0].get("confidence", {})
        if not isinstance(top_confidence, dict) or not top_confidence.get("sufficient_for_decision"):
            blocking_reasons.append("insufficient_confidence_for_top_opportunity")

    deduped_blocking_reasons = sorted(set(reason for reason in blocking_reasons if reason))
    readiness_ok = not deduped_blocking_reasons
    readiness_status = "ready" if readiness_ok else "blocked"
    decision = "RANKED" if readiness_ok else "NO_DECISION"

    ranked_opportunities = []
    if readiness_ok:
        for idx, row in enumerate(ranked_candidates, start=1):
            ranked_opportunities.append({"rank": idx, **row})

    return {
        "schema": "pi.perf.opportunity_matrix.v1",
        "bead_id": "bd-3ar8v.6.1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "run_id": run_id,
        "correlation_id": correlation_id,
        "source_identity": {
            "source_artifact": "phase1_matrix_validation",
            "source_artifact_path": str(source_artifact_path),
            "weighted_bottleneck_schema": weighted_schema,
            "weighted_bottleneck_status": weighted_status,
            "source_stream": source_stream,
            "source_cell_count": source_cell_count,
            "valid_cell_count": valid_cell_count,
        },
        "scoring_model": {
            "impact_metric": "weighted_contribution_pct",
            "confidence_metric": "ci95_width_pct + sample_size",
            "effort_metric": "stage_effort_points",
            "priority_formula": "priority_score = (expected_gain_pct * confidence_score) / effort_points",
            "expected_gain_formula": "expected_gain_pct = weighted_contribution_pct * 0.85",
        },
        "readiness": {
            "status": readiness_status,
            "decision": decision,
            "mode": "fail_closed",
            "ready_for_phase5": readiness_ok,
            "blocking_reasons": deduped_blocking_reasons,
            "fail_closed_conditions": [
                "weighted_bottleneck_attribution_not_computed",
                "phase1_matrix_not_ready_for_phase5",
                "missing_weighted_global_ranking",
                "insufficient_freshness_identity",
                "lineage_source_stream_mismatch",
                "missing_lineage_source_cell_count",
                "missing_lineage_valid_cell_count",
                "lineage_valid_cell_count_exceeds_source_count",
                "missing_rankable_opportunities",
                "insufficient_confidence_for_top_opportunity",
            ],
        },
        "ranked_opportunities": ranked_opportunities,
        "lineage": {
            "source_stream": "phase1_matrix_validation.weighted_bottleneck_attribution.global_ranking",
            "source_run_id": run_id,
            "source_correlation_id": correlation_id,
        },
    }

suite_logs = {}
for suite_name in ["perf_baseline_variance", "perf_regression", "perf_budgets"]:
    suite_dir = output_dir / "results" / suite_name
    suite_logs[suite_name] = {
        "stdout": str(suite_dir / "stdout.log"),
        "stderr": str(suite_dir / "stderr.log"),
        "result": str(suite_dir / "result.json"),
        "status": suite_status(suite_name, suite_result_by_name),
        "present": suite_dir.exists(),
    }

fault_injection_candidates = []
if fault_injection_root.exists():
    fault_injection_candidates = sorted(
        fault_injection_root.glob("*/integrity-summary.json")
    )
fault_injection_summary_path = None
fault_injection_status = "missing"
fault_injection_summary = {}
matching_fault_injection_summaries = []
for candidate in fault_injection_candidates:
    try:
        candidate_summary = load_json(candidate)
    except Exception:
        continue
    if not isinstance(candidate_summary, dict):
        continue
    if candidate_summary.get("schema") != "pi.e2e.persistence_fault_injection.summary.v1":
        continue
    if candidate_summary.get("correlation_id") != correlation_id:
        continue
    if candidate_summary.get("run_id") != correlation_id:
        continue
    if candidate_summary.get("source_commit") != source_commit:
        continue
    if candidate_summary.get("source_dirty") is not source_dirty:
        continue
    source_tree_digest = candidate_summary.get("source_tree_sha256")
    if not isinstance(source_tree_digest, str) or len(source_tree_digest) != 64:
        continue
    if any(character not in "0123456789abcdef" for character in source_tree_digest):
        continue
    if candidate_summary.get("source_commit_final") != source_commit:
        continue
    if candidate_summary.get("source_dirty_final") is not source_dirty:
        continue
    if candidate_summary.get("source_tree_sha256_final") != source_tree_digest:
        continue
    if candidate_summary.get("source_tree_stable") is not True:
        continue
    candidate_timestamp = parse_record_timestamp(candidate_summary)
    if candidate_timestamp is None:
        continue
    if candidate_timestamp < run_started_at - source_clock_skew:
        continue
    if candidate_timestamp > datetime.now(timezone.utc) + source_clock_skew:
        continue
    matching_fault_injection_summaries.append(
        (candidate_timestamp, str(candidate), candidate, candidate_summary)
    )

if matching_fault_injection_summaries:
    (
        _,
        _,
        fault_injection_summary_path,
        fault_injection_summary,
    ) = max(matching_fault_injection_summaries)
    fault_injection_status = (
        "pass" if fault_injection_summary.get("overall_passed") is True else "fail"
    )

memory_status = suite_status("perf_budgets", suite_result_by_name)
if memory_status == "pass":
    memory_status = "pass"
elif memory_status == "fail":
    memory_status = "fail"
else:
    memory_status = "missing"

correctness_status = suite_status("perf_regression", suite_result_by_name)
if correctness_status == "pass":
    correctness_status = "pass"
elif correctness_status == "fail":
    correctness_status = "fail"
else:
    correctness_status = "missing"

security_status = fault_injection_status

primary_outcome_missing = []
if primary_wall_clock_ms is None:
    primary_outcome_missing.append("missing_e2e_wall_clock_ms")
if primary_rust_vs_node_ratio is None:
    primary_outcome_missing.append("missing_rust_vs_node_ratio")
if primary_rust_vs_bun_ratio is None:
    primary_outcome_missing.append("missing_rust_vs_bun_ratio")

primary_status = "pass" if not primary_outcome_missing else "fail"

regression_guard_failures = []
for guard_name, status in (
    ("memory", memory_status),
    ("correctness", correctness_status),
    ("security", security_status),
):
    if status == "fail":
        regression_guard_failures.append(f"{guard_name}_regression")
    elif status == "missing":
        regression_guard_failures.append(f"{guard_name}_regression_unverified")

required_cell_count = len(required_partitions) * len(required_sizes)
phase5_ready = (
    source_lineage_valid
    and
    primary_status == "pass"
    and cells_with_complete_stage_breakdown == required_cell_count
    and len(missing_cells) == 0
    and cells_with_complete_swarm_metrics == required_cell_count
    and len(swarm_missing_cells) == 0
    and not any(status != "pass" for status in (memory_status, correctness_status, security_status))
)

payload = {
    "schema": "pi.perf.phase1_matrix_validation.v1",
    "bead_id": "bd-3ar8v.2.8",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "source_commit": source_commit,
    "source_dirty": source_dirty,
    "run_id": run_id,
    "correlation_id": correlation_id,
    "source_datasets": source_datasets,
    "matrix_requirements": {
        "required_partition_tags": required_partitions,
        "required_session_message_sizes": required_sizes,
        "required_cell_count": required_cell_count,
    },
    "matrix_cells": cells,
    "stage_summary": {
        "required_stage_keys": required_stage_keys,
        "operation_stage_coverage": operation_stage_coverage,
        "cells_with_complete_stage_breakdown": cells_with_complete_stage_breakdown,
        "cells_missing_stage_breakdown": required_cell_count
        - cells_with_complete_stage_breakdown,
        "covered_cells": covered_cells,
        "missing_cells": missing_cells,
    },
    "swarm_summary": {
        "required_latency_quantiles": required_latency_quantiles,
        "required_queue_depth_quantiles": required_queue_depth_quantiles,
        "required_resource_usage_keys": required_resource_usage_keys,
        "required_component_breakdown_keys": required_component_breakdown_keys,
        "required_stage_breakdown_keys": required_stage_breakdown_keys,
        "cells_with_complete_swarm_metrics": cells_with_complete_swarm_metrics,
        "cells_missing_swarm_metrics": required_cell_count
        - cells_with_complete_swarm_metrics,
        "missing_cells": swarm_missing_cells,
    },
    "weighted_bottleneck_attribution": weighted_bottleneck_attribution,
    "primary_outcomes": {
        "status": primary_status,
        "wall_clock_ms": primary_wall_clock_ms,
        "rust_vs_node_ratio": primary_rust_vs_node_ratio,
        "rust_vs_bun_ratio": primary_rust_vs_bun_ratio,
        "missing_reasons": primary_outcome_missing,
        "ordering_policy": "primary_e2e_before_microbench",
    },
    "regression_guards": {
        "memory": memory_status,
        "correctness": correctness_status,
        "security": security_status,
        "failure_or_gap_reasons": sorted(set(regression_guard_failures)),
    },
    "evidence_links": {
        "phase1_unit_and_fault_injection": {
            "suite_logs": suite_logs,
            "fault_injection_script": str(fault_injection_script),
            "fault_injection_summary_path": (
                str(fault_injection_summary_path)
                if fault_injection_summary_path is not None
                else None
            ),
        },
        "required_artifacts": {
            "scenario_runner": str(effective_scenario_runner_path),
            "workload": str(effective_workload_path),
            "stratification": str(stratification_path),
            "baseline_variance_confidence": str(baseline_path),
        },
        "source_identity": {
            "run_id": run_id,
            "correlation_id": correlation_id,
        },
    },
    "consumption_contract": {
        "downstream_beads": [
            "bd-3ar8v.2.12",
            "bd-3ar8v.6.1",
            "bd-3ar8v.6.2",
            "bd-3ar8v.6.6",
            "bd-3ar8v.6.11",
        ],
        "downstream_consumers": {
            "opportunity_matrix": {
                "bead_id": "bd-3ar8v.6.1",
                "selector": "weighted_bottleneck_attribution.global_ranking",
                "source_artifact": "phase1_matrix_validation",
            },
            "parameter_sweeps": {
                "bead_id": "bd-3ar8v.6.2",
                "selector": "weighted_bottleneck_attribution.per_scale",
                "source_artifact": "phase1_matrix_validation",
            },
        },
        "artifact_ready_for_phase5": phase5_ready,
        "fail_closed_conditions": [
            "missing_current_run_source",
            "mixed_source_lineage",
            "missing_matrix_source_record",
            "missing_stage_metrics",
            "missing_primary_wall_clock",
            "missing_primary_relative_ratios",
            "missing_swarm_metrics",
            "memory_regression",
            "correctness_regression",
            "security_regression",
        ],
    },
    "lineage": {
        "run_id_lineage": [run_id, correlation_id],
        "source_manifest_path": str(manifest_path),
        "source_scenario_runner_path": str(effective_scenario_runner_path),
        "source_workload_path": str(effective_workload_path),
        "source_stratification_path": str(stratification_path),
        "source_baseline_confidence_path": str(baseline_path),
        "source_perf_sli_contract_path": str(perf_sli_path),
    },
}

parameter_sweeps_artifact = compute_parameter_sweeps_artifact(
    weighted_bottleneck_attribution,
    required_sizes,
    required_partitions,
    phase5_ready,
    phase1_matrix_path,
)
opportunity_matrix_artifact = compute_opportunity_matrix_artifact(
    weighted_bottleneck_attribution,
    phase5_ready,
    phase1_matrix_path,
)

phase1_matrix_path.parent.mkdir(parents=True, exist_ok=True)
phase1_matrix_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
parameter_sweeps_path.parent.mkdir(parents=True, exist_ok=True)
parameter_sweeps_path.write_text(
    json.dumps(parameter_sweeps_artifact, indent=2) + "\n",
    encoding="utf-8",
)
opportunity_matrix_path.parent.mkdir(parents=True, exist_ok=True)
opportunity_matrix_path.write_text(
    json.dumps(opportunity_matrix_artifact, indent=2) + "\n",
    encoding="utf-8",
)

manifest["phase1_matrix_validation"] = {
    "schema": "pi.perf.phase1_matrix_validation.v1",
    "path": str(phase1_matrix_path),
    "required_cell_count": required_cell_count,
    "covered_cell_count": covered_cells,
    "cells_with_complete_stage_breakdown": cells_with_complete_stage_breakdown,
    "cells_with_complete_swarm_metrics": cells_with_complete_swarm_metrics,
    "artifact_ready_for_phase5": phase5_ready,
}
manifest["parameter_sweeps"] = {
    "schema": "pi.perf.parameter_sweeps.v1",
    "path": str(parameter_sweeps_path),
    "status": parameter_sweeps_artifact.get("readiness", {}).get("status"),
    "ready_for_phase5": parameter_sweeps_artifact.get("readiness", {}).get("ready_for_phase5"),
    "top_stage": parameter_sweeps_artifact.get("sensitivity_summary", {}).get("top_stage"),
}
manifest["opportunity_matrix"] = {
    "schema": "pi.perf.opportunity_matrix.v1",
    "path": str(opportunity_matrix_path),
    "status": opportunity_matrix_artifact.get("readiness", {}).get("status"),
    "decision": opportunity_matrix_artifact.get("readiness", {}).get("decision"),
    "ready_for_phase5": opportunity_matrix_artifact.get("readiness", {}).get("ready_for_phase5"),
    "ranked_count": len(opportunity_matrix_artifact.get("ranked_opportunities", [])),
    "top_stage": (
        opportunity_matrix_artifact.get("ranked_opportunities", [{}])[0].get("stage")
        if opportunity_matrix_artifact.get("ranked_opportunities")
        else None
    ),
}
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY
then
  artifact_count=$((artifact_count + 3))
  log_ok "Phase-1 matrix validation written: results/phase1_matrix_validation.json"
  log_ok "Parameter sweeps written: results/parameter_sweeps.json"
  log_ok "Opportunity matrix written: results/opportunity_matrix.json"
else
  die "Failed to generate phase-1 matrix validation artifact"
fi

# ─── Phase 5g: Authoritative post-generation evidence gate ─────────────────

log_phase "Phase 5g: Post-Generation Evidence Gate"

POST_GENERATION_CONTRACT_PATH="$OUTPUT_DIR/results/post_generation_evidence_contract.json"
post_generation_exit=0
if OUTPUT_DIR="$OUTPUT_DIR" \
  CORRELATION_ID="$CORRELATION_ID" \
  GIT_COMMIT_FULL="$GIT_COMMIT_FULL" \
  GIT_DIRTY="$GIT_DIRTY" \
  POST_GENERATION_CONTRACT_PATH="$POST_GENERATION_CONTRACT_PATH" \
  python3 - <<'PY'
import hashlib
import json
import math
import os
from datetime import datetime, timezone
from pathlib import Path

output_dir = Path(os.environ["OUTPUT_DIR"])
expected_correlation_id = os.environ["CORRELATION_ID"]
expected_source_commit = os.environ["GIT_COMMIT_FULL"]
expected_source_dirty = os.environ["GIT_DIRTY"] == "true"
report_path = Path(os.environ["POST_GENERATION_CONTRACT_PATH"])
phase1_path = output_dir / "results" / "phase1_matrix_validation.json"
stratification_path = output_dir / "results" / "extension_benchmark_stratification.json"
failures = []


def load_artifact(path: Path, expected_schema: str):
    if not path.is_file():
        failures.append({"path": str(path), "reason": "missing_artifact"})
        return {}
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        failures.append(
            {"path": str(path), "reason": "invalid_json", "detail": str(error)}
        )
        return {}
    if payload.get("schema") != expected_schema:
        failures.append(
            {
                "path": str(path),
                "reason": "schema_mismatch",
                "expected": expected_schema,
                "observed": payload.get("schema"),
            }
        )
    for field, expected in (
        ("source_commit", expected_source_commit),
        ("source_dirty", expected_source_dirty),
        ("correlation_id", expected_correlation_id),
    ):
        if payload.get(field) != expected:
            failures.append(
                {
                    "path": str(path),
                    "reason": f"{field}_mismatch",
                    "expected": expected,
                    "observed": payload.get(field),
                }
            )
    return payload


def validate_source_datasets(path: Path, payload):
    datasets = payload.get("source_datasets", [])
    if not isinstance(datasets, list) or not datasets:
        failures.append({"path": str(path), "reason": "missing_source_datasets"})
        return
    for dataset in datasets:
        if not isinstance(dataset, dict):
            failures.append({"path": str(path), "reason": "invalid_source_dataset"})
            continue
        accepted = dataset.get("accepted_record_count")
        rejected = dataset.get("rejected_record_count")
        required = dataset.get("required", True)
        source_path = dataset.get("path")
        expected_sha256 = dataset.get("sha256")
        verify_source_bytes = (
            required
            or (isinstance(accepted, int) and accepted > 0)
            or (isinstance(rejected, int) and rejected > 0)
            or expected_sha256 is not None
        )
        if required and (not isinstance(accepted, int) or accepted <= 0):
            failures.append(
                {
                    "path": str(path),
                    "reason": "missing_current_run_source_records",
                    "source_path": dataset.get("path"),
                }
            )
        if verify_source_bytes and (not isinstance(source_path, str) or not source_path):
            failures.append(
                {"path": str(path), "reason": "missing_source_dataset_path"}
            )
        elif verify_source_bytes and not Path(source_path).is_file():
            failures.append(
                {
                    "path": str(path),
                    "reason": "source_dataset_missing_after_derivation",
                    "source_path": source_path,
                }
            )
        elif verify_source_bytes:
            observed_sha256 = hashlib.sha256(Path(source_path).read_bytes()).hexdigest()
            if not isinstance(expected_sha256, str) or observed_sha256 != expected_sha256:
                failures.append(
                    {
                        "path": str(path),
                        "reason": "source_dataset_checksum_mismatch",
                        "source_path": source_path,
                        "expected_sha256": expected_sha256,
                        "observed_sha256": observed_sha256,
                    }
                )
        if not isinstance(rejected, int) or rejected != 0:
            failures.append(
                {
                    "path": str(path),
                    "reason": "mixed_source_lineage",
                    "source_path": dataset.get("path"),
                    "rejected_record_count": rejected,
                }
            )


phase1 = load_artifact(phase1_path, "pi.perf.phase1_matrix_validation.v1")
validate_source_datasets(phase1_path, phase1)
if phase1.get("consumption_contract", {}).get("artifact_ready_for_phase5") is not True:
    failures.append({"path": str(phase1_path), "reason": "phase1_not_ready"})
matrix_cells = phase1.get("matrix_cells", [])
required_cell_count = phase1.get("matrix_requirements", {}).get("required_cell_count")
if not isinstance(required_cell_count, int) or required_cell_count <= 0:
    failures.append({"path": str(phase1_path), "reason": "invalid_required_cell_count"})
elif not isinstance(matrix_cells, list) or len(matrix_cells) != required_cell_count:
    failures.append(
        {
            "path": str(phase1_path),
            "reason": "matrix_cell_count_mismatch",
            "expected": required_cell_count,
            "observed": len(matrix_cells) if isinstance(matrix_cells, list) else None,
        }
    )
if isinstance(matrix_cells, list):
    required_stages = ("open_ms", "append_ms", "save_ms", "index_ms")
    for index, cell in enumerate(matrix_cells):
        attribution = cell.get("stage_attribution", {}) if isinstance(cell, dict) else {}
        invalid_stages = [
            key
            for key in required_stages
            if not isinstance(attribution.get(key), (int, float))
            or not math.isfinite(float(attribution[key]))
            or float(attribution[key]) < 0.0
        ]
        if (
            not isinstance(cell, dict)
            or cell.get("status") != "pass"
            or cell.get("missing_reasons") not in ([], None)
            or invalid_stages
        ):
            failures.append(
                {
                    "path": str(phase1_path),
                    "reason": "invalid_matrix_cell",
                    "cell_index": index,
                    "invalid_stages": invalid_stages,
                }
            )

stratification = load_artifact(
    stratification_path, "pi.perf.extension_benchmark_stratification.v1"
)
validate_source_datasets(stratification_path, stratification)
required_layers = {"cold_load_init", "per_call_dispatch_micro", "full_e2e_long_session"}
layers = stratification.get("layers", [])
observed_layers = {
    layer.get("layer_id")
    for layer in layers
    if isinstance(layer, dict) and layer.get("evidence_state") == "measured"
}
if observed_layers != required_layers:
    failures.append(
        {
            "path": str(stratification_path),
            "reason": "required_layers_not_measured",
            "expected": sorted(required_layers),
            "observed": sorted(layer for layer in observed_layers if isinstance(layer, str)),
        }
    )
claim_guard = stratification.get("claim_integrity", {}).get("cherry_pick_guard", {})
if claim_guard.get("global_claim_valid") is not True:
    failures.append(
        {"path": str(stratification_path), "reason": "global_claim_not_valid"}
    )

report = {
    "schema": "pi.perf.post_generation_evidence_contract.v1",
    "generated_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    "source_commit": expected_source_commit,
    "source_dirty": expected_source_dirty,
    "correlation_id": expected_correlation_id,
    "status": "ready" if not failures else "blocked",
    "failure_count": len(failures),
    "failures": failures,
    "artifacts": [str(phase1_path), str(stratification_path)],
}
report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
raise SystemExit(0 if not failures else 1)
PY
then
  log_ok "Post-generation evidence contract passed"
else
  post_generation_exit=$?
  log_warn "Post-generation evidence contract blocked: results/$(basename "$POST_GENERATION_CONTRACT_PATH")"
fi
artifact_count=$((artifact_count + 1))

POST_GENERATION_BUDGET_DIR="$OUTPUT_DIR/results/perf_budgets_post_generation"
post_generation_budget_exit=0
POST_GENERATION_EVIDENCE_DIR="$OUTPUT_DIR/results"
declare -a POST_GENERATION_RUNNER_ARGS=("${CARGO_RUNNER_ARGS[@]}")
if [[ "$CARGO_RUNNER_MODE" == "rch" ]]; then
  for required_env in PERF_EVIDENCE_DIR PI_PERF_POST_GENERATION; do
    case ",${RCH_ENV_ALLOWLIST:-}," in
      *",$required_env,"*) ;;
      *) RCH_ENV_ALLOWLIST="${RCH_ENV_ALLOWLIST:+$RCH_ENV_ALLOWLIST,}$required_env" ;;
    esac
  done
  export RCH_ENV_ALLOWLIST

  post_generation_stage_key="$({ printf '%s\0' "$CORRELATION_ID" "$TIMESTAMP" "$$"; } | sha256sum | cut -d' ' -f1)"
  POST_GENERATION_STAGE_RELATIVE=".rch-tmp/pi-perf-evidence/$post_generation_stage_key"
  POST_GENERATION_STAGE_ABSOLUTE="$PROJECT_ROOT/$POST_GENERATION_STAGE_RELATIVE"
  if [[ -e "$POST_GENERATION_STAGE_ABSOLUTE" \
    || -L "$POST_GENERATION_STAGE_ABSOLUTE" ]]; then
    die "Refusing preexisting post-generation RCH evidence stage: $POST_GENERATION_STAGE_RELATIVE"
  fi
  mkdir -p "$POST_GENERATION_STAGE_ABSOLUTE"
  unsafe_stage_entry=""
  while IFS= read -r -d '' candidate; do
    unsafe_stage_entry="$candidate"
    break
  done < <(find "$OUTPUT_DIR/results" -type l -print0 2>/dev/null)
  if [[ -n "$unsafe_stage_entry" ]]; then
    die "Refusing symlink in post-generation RCH evidence: $unsafe_stage_entry"
  fi
  while IFS= read -r -d '' candidate; do
    unsafe_stage_entry="$candidate"
    break
  done < <(find "$OUTPUT_DIR/results" ! -type d ! -type f -print0 2>/dev/null)
  if [[ -n "$unsafe_stage_entry" ]]; then
    die "Refusing non-regular post-generation RCH evidence: $unsafe_stage_entry"
  fi
  staged_file_count=0
  while IFS= read -r -d '' evidence_file; do
    evidence_relative="${evidence_file#"$OUTPUT_DIR/results/"}"
    if [[ "$evidence_relative" == "$evidence_file" || -z "$evidence_relative" ]]; then
      die "Could not derive post-generation evidence-relative path: $evidence_file"
    fi
    evidence_destination="$POST_GENERATION_STAGE_ABSOLUTE/$evidence_relative"
    mkdir -p "$(dirname "$evidence_destination")"
    cp "$evidence_file" "$evidence_destination"
    staged_file_count=$((staged_file_count + 1))
  done < <(find "$OUTPUT_DIR/results" -type f -print0 2>/dev/null)
  if [[ "$staged_file_count" -eq 0 ]]; then
    die "Post-generation RCH evidence stage contains no regular files"
  fi
  POST_GENERATION_EVIDENCE_DIR="$POST_GENERATION_STAGE_RELATIVE"
  POST_GENERATION_RUNNER_ARGS=(
    "rch" "exec"
    "--no-color"
    "--base" "$GIT_COMMIT_FULL"
    "--clean-overlay"
    "--overlay-path" "$POST_GENERATION_STAGE_RELATIVE"
    "--" "cargo"
  )
fi
mkdir -p "$POST_GENERATION_BUDGET_DIR"
PERF_EVIDENCE_DIR="$POST_GENERATION_EVIDENCE_DIR" \
PI_PERF_POST_GENERATION=1 \
CI_CORRELATION_ID="$CORRELATION_ID" \
RCH_QUIET=0 \
RCH_VISIBILITY=summary \
"${POST_GENERATION_RUNNER_ARGS[@]}" test --test perf_budgets --profile "$CARGO_PROFILE" \
  ci_enforced_budgets_fail_on_regression_or_missing_data -- --exact --nocapture \
  > "$POST_GENERATION_BUDGET_DIR/stdout.log" \
  2> "$POST_GENERATION_BUDGET_DIR/stderr.log" \
  || post_generation_budget_exit=$?

if [[ "$post_generation_budget_exit" -eq 0 && "$CARGO_RUNNER_MODE" == "rch" ]]; then
  if ! grep -Eqs '^\[RCH\] remote [^[:space:]]+ \([^)]+\)$' \
    "$POST_GENERATION_BUDGET_DIR/stdout.log" \
    "$POST_GENERATION_BUDGET_DIR/stderr.log"; then
    log_fail "Post-generation perf budget gate has no remote-success marker"
    post_generation_budget_exit=97
  elif grep -Eqs '^\[RCH\] local( |$)' \
    "$POST_GENERATION_BUDGET_DIR/stdout.log" \
    "$POST_GENERATION_BUDGET_DIR/stderr.log"; then
    log_fail "Post-generation perf budget gate reported local execution"
    post_generation_budget_exit=98
  elif ! grep -Eqs \
    "^\\[RCH\\] clean-overlay receipt: base=$GIT_COMMIT_FULL overlay-fingerprint=[0-9a-f]{64}$" \
    "$POST_GENERATION_BUDGET_DIR/stdout.log" \
    "$POST_GENERATION_BUDGET_DIR/stderr.log"; then
    log_fail "Post-generation perf budget gate has no current-commit clean-overlay receipt"
    post_generation_budget_exit=99
  fi
fi

post_generation_budget_status="pass"
if [[ "$post_generation_budget_exit" -eq 0 ]]; then
  suite_pass=$((suite_pass + 1))
  log_ok "Post-generation perf budget data-contract evaluation passed"
elif [[ "${PI_PERF_STRICT:-0}" == "1" ]]; then
  post_generation_budget_status="fail"
  suite_fail=$((suite_fail + 1))
  log_warn "Post-generation perf budget data-contract evaluation failed (exit=$post_generation_budget_exit)"
else
  post_generation_budget_status="skip"
  suite_skip=$((suite_skip + 1))
  log_warn "Post-generation perf budget data-contract evaluation skipped after failure (exit=$post_generation_budget_exit)"
fi

staging_exit=0
if run_budget_preflight "$PREFLIGHT_AFTER_RUN_PATH" --artifact-readiness-only; then
  log_ok "Final budget preflight passed: results/$(basename "$PREFLIGHT_AFTER_RUN_PATH")"
else
  staging_exit=$?
  log_warn "Final budget preflight found blockers:"
  log_warn "  results/$(basename "$PREFLIGHT_AFTER_RUN_PATH") (exit=$staging_exit)"
fi

if [[ -f "$PREFLIGHT_AFTER_RUN_PATH" ]]; then
  artifact_count=$((artifact_count + 1))
  log_ok "Collected: $(basename "$PREFLIGHT_AFTER_RUN_PATH")"
fi

if run_artifact_staging_manifest "$STAGING_MANIFEST_PATH"; then
  log_ok "Final artifact staging passed: results/$(basename "$STAGING_MANIFEST_PATH")"
else
  staging_exit=$?
  log_warn "Final artifact staging found blockers: results/$(basename "$STAGING_MANIFEST_PATH") (exit=$staging_exit)"
fi

if [[ -f "$STAGING_MANIFEST_PATH" ]]; then
  artifact_count=$((artifact_count + 1))
  staging_summary="$(
    python3 - "$STAGING_MANIFEST_PATH" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
summary = payload.get("summary", {})
print(
    "|".join(
        str(summary.get(key, 0))
        for key in (
            "status",
            "missing_required_count",
            "stale_required_count",
            "present_required_count",
        )
    )
    + "|"
    + str(len(payload.get("blockers", [])))
)
PY
  )"
  IFS='|' read -r \
    ARTIFACT_STAGING_STATUS \
    ARTIFACT_STAGING_MISSING_REQUIRED \
    ARTIFACT_STAGING_STALE_REQUIRED \
    ARTIFACT_STAGING_PRESENT_REQUIRED \
    ARTIFACT_STAGING_BLOCKERS <<< "$staging_summary"
  log_ok "Final artifact staging: status=$ARTIFACT_STAGING_STATUS present=$ARTIFACT_STAGING_PRESENT_REQUIRED"
  log_ok "Final artifact blockers: missing=$ARTIFACT_STAGING_MISSING_REQUIRED stale=$ARTIFACT_STAGING_STALE_REQUIRED"
else
  log_warn "Final artifact staging manifest was not generated"
fi

post_generation_status="pass"
post_generation_result_exit=0
if [[ "$post_generation_exit" -ne 0 \
  || "$post_generation_budget_exit" -ne 0 \
  || "$staging_exit" -ne 0 \
  || "$ARTIFACT_STAGING_STATUS" == "blocked" ]]; then
  if [[ "$post_generation_exit" -ne 0 ]]; then
    post_generation_result_exit="$post_generation_exit"
  elif [[ "$post_generation_budget_exit" -ne 0 ]]; then
    post_generation_result_exit="$post_generation_budget_exit"
  else
    post_generation_result_exit="$staging_exit"
  fi
  if [[ "${PI_PERF_STRICT:-0}" == "1" ]]; then
    post_generation_status="fail"
    suite_fail=$((suite_fail + 1))
  else
    post_generation_status="skip"
    suite_skip=$((suite_skip + 1))
  fi
else
  suite_pass=$((suite_pass + 1))
fi

OUTPUT_DIR="$OUTPUT_DIR" \
  ARTIFACT_COUNT="$artifact_count" \
  SUITE_PASS="$suite_pass" \
  SUITE_FAIL="$suite_fail" \
  SUITE_SKIP="$suite_skip" \
  POST_GENERATION_STATUS="$post_generation_status" \
  POST_GENERATION_EXIT="$post_generation_result_exit" \
  POST_GENERATION_BUDGET_STATUS="$post_generation_budget_status" \
  POST_GENERATION_BUDGET_EXIT="$post_generation_budget_exit" \
  ARTIFACT_STAGING_STATUS="$ARTIFACT_STAGING_STATUS" \
  ARTIFACT_STAGING_MISSING_REQUIRED="$ARTIFACT_STAGING_MISSING_REQUIRED" \
  ARTIFACT_STAGING_STALE_REQUIRED="$ARTIFACT_STAGING_STALE_REQUIRED" \
  ARTIFACT_STAGING_BLOCKERS="$ARTIFACT_STAGING_BLOCKERS" \
  python3 - <<'PY'
import json
import os
from pathlib import Path

manifest_path = Path(os.environ["OUTPUT_DIR"]) / "manifest.json"
manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
run_summary = manifest.setdefault("run_summary", {})
passed = int(os.environ["SUITE_PASS"])
failed = int(os.environ["SUITE_FAIL"])
skipped = int(os.environ["SUITE_SKIP"])
run_summary.update(
    {
        "total_suites": passed + failed + skipped,
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "artifact_count": int(os.environ["ARTIFACT_COUNT"]),
    }
)
manifest.setdefault("artifact_staging", {}).update(
    {
        "status": os.environ["ARTIFACT_STAGING_STATUS"],
        "missing_required_count": int(os.environ["ARTIFACT_STAGING_MISSING_REQUIRED"]),
        "stale_required_count": int(os.environ["ARTIFACT_STAGING_STALE_REQUIRED"]),
        "blocker_count": int(os.environ["ARTIFACT_STAGING_BLOCKERS"]),
    }
)
suite_results = manifest.setdefault("suite_results", [])
suite_results.append(
    {
        "suite": "perf_budgets_post_generation",
        "status": os.environ["POST_GENERATION_BUDGET_STATUS"],
        "exit_code": int(os.environ["POST_GENERATION_BUDGET_EXIT"]),
        "elapsed_ms": 0,
    }
)
suite_results.append(
    {
        "suite": "post_generation_evidence",
        "status": os.environ["POST_GENERATION_STATUS"],
        "exit_code": int(os.environ["POST_GENERATION_EXIT"]),
        "elapsed_ms": 0,
    }
)
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY

log_ok "Total artifacts collected and finalized: $artifact_count"

# ─── Phase 6: Generate checksums ────────────────────────────────────────────

log_phase "Phase 6: Integrity Checksums"

pushd "$OUTPUT_DIR" >/dev/null
# Checksum all result files
find results/ -type f \( -name "*.json" -o -name "*.jsonl" -o -name "*.log" \) 2>/dev/null \
  | sort \
  | while IFS= read -r file; do
    sha256sum "$file"
  done > checksums.sha256

# Also checksum the manifest and fingerprint
sha256sum manifest.json >> checksums.sha256
if [[ -f env_fingerprint.json ]]; then
  sha256sum env_fingerprint.json >> checksums.sha256
fi
popd >/dev/null

checksum_count=$(wc -l < "$OUTPUT_DIR/checksums.sha256")
log_ok "Generated $checksum_count checksums"

# ─── Phase 7: Bundle (optional) ─────────────────────────────────────────────

if [[ "$CREATE_BUNDLE" -eq 1 ]]; then
  log_phase "Phase 7: Create Artifact Bundle"

  bundle_name="perf-bundle-${TIMESTAMP}-${GIT_COMMIT}"
  bundle_path="$TARGET_DIR/perf/bundles/${bundle_name}.tar.gz"
  mkdir -p "$(dirname "$bundle_path")"

  tar -czf "$bundle_path" -C "$(dirname "$OUTPUT_DIR")" "$(basename "$OUTPUT_DIR")"
  bundle_size=$(du -h "$bundle_path" | cut -f1)
  bundle_sha=$(sha256_file "$bundle_path")

  log_ok "Bundle created: $bundle_path ($bundle_size)"
  log_ok "Bundle SHA-256: $bundle_sha"

  # Write bundle metadata alongside the archive
  cat > "${bundle_path%.tar.gz}.meta.json" <<EOF
{
  "schema": "pi.perf.bundle_meta.v1",
  "bundle_name": "$bundle_name",
  "bundle_path": "$bundle_path",
  "bundle_sha256": "$bundle_sha",
  "source_dir": "$OUTPUT_DIR",
  "correlation_id": "$CORRELATION_ID",
  "created_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF
fi

# ─── Summary ─────────────────────────────────────────────────────────────────

log_phase "Summary"

echo "  Suites:       $((suite_pass + suite_fail + suite_skip)) total ($suite_pass pass, $suite_fail fail, $suite_skip skip)"
echo "  Artifacts:    $artifact_count collected"
echo "  Checksums:    $checksum_count verified"
echo "  Duration:     ${run_elapsed}ms"
echo "  Output:       $OUTPUT_DIR"
echo "  Manifest:     $OUTPUT_DIR/manifest.json"
echo "  Correlation:  $CORRELATION_ID"

if [[ "$suite_fail" -gt 0 ]]; then
  echo ""
  log_warn "$suite_fail suite(s) failed — check results/ for details"
  exit 1
fi

green "All suites passed."
