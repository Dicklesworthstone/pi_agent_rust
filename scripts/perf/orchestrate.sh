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
  [criterion_pijs]="pijs_workload"
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
if [[ "$RCH_PROOF_REQUIRED" == true && "$CARGO_RUNNER_REQUEST" != "rch" ]]; then
  die "RCH proof mode requires PERF_CARGO_RUNNER=rch; local and auto runners cannot prove remote execution"
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
    PI_BENCH_CORRELATION_ID \
    PI_BENCH_ALLOCATOR \
    PI_BENCH_MODE \
    PI_BENCH_LEGACY_RUNTIMES \
    CARGO_BUILD_JOBS \
    PERF_REGRESSION_OUTPUT \
    PERF_REGRESSION_FULL \
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
    PI_CRITERION_OUTPUT_SUBDIR \
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
    full)
      export PI_PERF_STRICT=1
      export PERF_REGRESSION_FULL=1
      export BENCH_QUICK=0
      ;;
    ci) export PI_PERF_STRICT=1 ;;
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

exclusive_post_generation_suite_set_selected() {
  local required_suite
  for required_suite in \
    "${!SUITE_TARGETS[@]}" \
    criterion_extensions \
    criterion_pijs \
    criterion_system \
    criterion_semantic_context; do
    if ! suite_selected "$required_suite"; then
      return 1
    fi
  done
  return 0
}

RUN_EXCLUSIVE_POST_GENERATION_GATE=false
post_generation_skip_reason="incomplete_full_evidence_suite_set"
if exclusive_post_generation_suite_set_selected; then
  if [[ "$PROFILE" != "full" ]]; then
    die "The full evidence suite set requires --profile full for the exclusive post-generation gate"
  fi
  if [[ "$CARGO_RUNNER_MODE" != "rch" ]]; then
    die "The full evidence suite set requires RCH for the exclusive post-generation gate"
  fi
  if [[ "$SKIP_BUILD" != "0" ]]; then
    die "The full evidence suite set cannot claim exclusive post-generation evidence with --skip-build"
  fi
  if [[ -n "${BENCH_ITERATIONS:-}" ]]; then
    die "The exclusive post-generation gate forbids BENCH_ITERATIONS overrides"
  fi
  for ext_bench_override in PI_BENCH_MAX PI_BENCH_ITERATIONS PI_BENCH_EVENT_COUNT; do
    if [[ -v $ext_bench_override ]]; then
      die "The exclusive post-generation gate forbids $ext_bench_override overrides"
    fi
  done
  if [[ "${BENCH_QUICK:-0}" != "0" || "${PERF_REGRESSION_FULL:-0}" != "1" ]]; then
    die "The exclusive post-generation gate requires canonical full benchmark settings"
  fi
  RUN_EXCLUSIVE_POST_GENERATION_GATE=true
  post_generation_skip_reason=""
  RCH_PROOF_REQUIRED=true
  export RCH_REQUIRE_REMOTE=1
fi

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

if [[ "$CARGO_RUNNER_MODE" == "rch" ]]; then
  if suite_selected "perf_bench_harness" && [[ "$CARGO_PROFILE" != "perf" ]]; then
    die "RCH extension benchmark proof requires PERF_PROFILE=perf, got: $CARGO_PROFILE"
  fi
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

declare -a PHASE2_RUNNER_ARGS=("${CARGO_RUNNER_ARGS[@]}")
if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
  PHASE2_RUNNER_ARGS=("${PERF_BENCH_RUNNER_ARGS[@]}")
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
    "$TARGET_DIR/criterion/pi-perf-runs/$RUN_INSTANCE_ID/criterion_extensions" \
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
if [[ ! "$CORRELATION_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$ ]]; then
  die "CI_CORRELATION_ID must be 1-128 path-safe characters ([A-Za-z0-9._:-]) and start with an alphanumeric character"
fi
RUN_INSTANCE_ID="$({ printf '%s\0' "$CORRELATION_ID" "$TIMESTAMP" "$$"; } | sha256sum | cut -d' ' -f1)"

# ─── Setup output directory ─────────────────────────────────────────────────

mkdir -p "$OUTPUT_DIR/results"
mkdir -p "$OUTPUT_DIR/logs"

log_phase "Perf Orchestrator v1.0 (bd-3ar8v.1.8)"
log_step "Profile:        $PROFILE"
log_step "Output:         $OUTPUT_DIR"
log_step "Correlation ID: $CORRELATION_ID"
log_step "Run instance:   $RUN_INSTANCE_ID"
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
import math
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
    "${PHASE2_RUNNER_ARGS[@]}" test --no-run --profile "$CARGO_PROFILE" \
      --message-format=json-render-diagnostics \
      >"$BUILD_TESTS_JSONL_PATH" \
      2>"$OUTPUT_DIR/logs/build_tests.log"; then
    log_ok "Test binaries built"
  else
    log_warn "Test binary build had warnings (see logs/build_tests.log)"
  fi

  # Build criterion benches if needed
  if [[ "$SKIP_CRITERION" != "1" ]]; then
    log_step "Building criterion benchmarks..."
    for bench in "${!CRITERION_BENCHES[@]}"; do
      bench_name="${CRITERION_BENCHES[$bench]}"
      bench_build_args=(bench --bench "$bench_name" --no-run --profile "$CARGO_PROFILE")
      if [[ "$bench" == "criterion_pijs" ]]; then
        bench_build_args+=(
          --no-default-features
          --features clipboard,image,image-resize,sqlite-sessions,tui,wasm-host
        )
      fi
      if "${PHASE2_RUNNER_ARGS[@]}" "${bench_build_args[@]}" 2>>"$OUTPUT_DIR/logs/build_benches.log"; then
        log_ok "Built bench: $bench_name"
      else
        log_warn "Build warning for bench: $bench_name"
      fi
    done
  fi

  if suite_selected "perf_budgets" || suite_selected "perf_regression"; then
    release_pi_built=0
    log_step "Building release pi binary for release-size gates..."
    if "${PHASE2_RUNNER_ARGS[@]}" build --bin pi --release >"$OUTPUT_DIR/logs/build_release_pi.log" 2>&1; then
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
        "${PHASE2_RUNNER_ARGS[@]}" bench --bench system --profile release -- __idle_rss_control__ \
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
  local expected_mode="$6"
  python3 - \
    "$artifact_path" \
    "$expected_profile" \
    "$expected_commit" \
    "$expected_correlation_id" \
    "$expected_benchmark_run_id" \
    "$expected_mode" <<'PY'
import hashlib
import json
import math
import re
import sys
from pathlib import Path

artifact_path = Path(sys.argv[1])
expected_profile = sys.argv[2]
expected_commit = sys.argv[3]
expected_correlation_id = sys.argv[4]
expected_benchmark_run_id = sys.argv[5]
expected_mode = sys.argv[6]
expected_extensions = (
    ["hello", "pirate", "diff"]
    if expected_mode == "quick"
    else [
        "hello",
        "pirate",
        "diff",
        "bookmark",
        "custom-header",
        "custom-footer",
        "confirm-destructive",
        "dirty-repo-guard",
    ]
)
expected_coverage = {
    (extension, scenario)
    for extension in expected_extensions
    for scenario in ("cold_start", "warm_start")
}
expected_coverage.update({("hello", "tool_call"), ("pirate", "event_hook")})
observed_coverage = set()
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
    if environment.get("debug_assertions") is not False:
        raise SystemExit(f"line {line_number}: perf build must disable debug assertions")
    features = environment.get("features")
    if (
        not isinstance(features, list)
        or not all(isinstance(feature, str) and feature for feature in features)
        or features != sorted(set(features))
    ):
        raise SystemExit(f"line {line_number}: compiled feature set is invalid")
    binary_path = environment.get("binary_path")
    binary_sha256 = environment.get("binary_sha256")
    if not isinstance(binary_path, str) or not binary_path.strip():
        raise SystemExit(f"line {line_number}: binary_path is missing")
    binary = Path(binary_path)
    if not binary.is_absolute():
        raise SystemExit(f"line {line_number}: binary_path must be absolute")
    binary_parent = binary.parent
    executable_profile_from_path = (
        binary_parent.parent.name
        if binary_parent.name in {"deps", "examples"}
        else binary_parent.name
    )
    if executable_profile_from_path != expected_profile:
        raise SystemExit(
            f"line {line_number}: binary path does not identify profile {expected_profile}"
        )
    if not isinstance(binary_sha256, str) or re.fullmatch(r"[0-9a-f]{64}", binary_sha256) is None:
        raise SystemExit(f"line {line_number}: binary_sha256 is invalid")
    canonical_provenance = {
        "binary_path": binary_path,
        "binary_sha256": binary_sha256,
        "build_fingerprint_contract": environment.get("build_fingerprint_contract"),
        "build_fingerprint_verified": environment.get("build_fingerprint_verified"),
        "build_profile": environment.get("build_profile"),
        "build_profile_verified": environment.get("build_profile_verified"),
        "compiled_debug": environment.get("compiled_debug"),
        "compiled_features": features,
        "compiled_opt_level": environment.get("compiled_opt_level"),
        "compiled_profile_family": environment.get("compiled_profile_family"),
        "debug_assertions": environment.get("debug_assertions"),
        "executable_build_profile": environment.get("executable_build_profile"),
        "executable_profile_verified": environment.get("executable_profile_verified"),
        "source_commit": environment.get("git_commit"),
        "source_dirty": environment.get("source_dirty"),
    }
    expected_config_hash = hashlib.sha256(
        json.dumps(canonical_provenance, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    if environment.get("config_hash") != expected_config_hash:
        raise SystemExit(f"line {line_number}: config_hash does not bind provenance fields")
    summary = record.get("summary")
    runs = record.get("runs")
    count = summary.get("count") if isinstance(summary, dict) else None
    if type(runs) is not int or runs <= 0 or type(count) is not int or count <= 0:
        raise SystemExit(f"line {line_number}: runs and summary.count must be positive integers")
    if count != runs:
        raise SystemExit(f"line {line_number}: runs and summary.count differ")
    summary_values = [
        summary.get(field)
        for field in ("min_ms", "p50_ms", "p95_ms", "p99_ms", "p999_ms", "max_ms", "mean_ms")
    ]
    if any(
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value < 0
        for value in summary_values
    ):
        raise SystemExit(f"line {line_number}: summary timings must be finite non-negative numbers")
    minimum, p50, p95, p99, p999, maximum, mean = summary_values
    if not minimum <= p50 <= p95 <= p99 <= p999 <= maximum or not minimum <= mean <= maximum:
        raise SystemExit(f"line {line_number}: summary timing order is invalid")
    for field in ("elapsed_ms", "per_call_us", "calls_per_sec"):
        value = record.get(field)
        if (
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not math.isfinite(value)
            or value < 0
        ):
            raise SystemExit(f"line {line_number}: {field} must be a finite non-negative number")
    coverage_key = (record.get("extension"), record.get("scenario"))
    if coverage_key not in expected_coverage:
        raise SystemExit(f"line {line_number}: unexpected benchmark coverage {coverage_key!r}")
    if coverage_key in observed_coverage:
        raise SystemExit(f"line {line_number}: duplicate benchmark coverage {coverage_key!r}")
    observed_coverage.add(coverage_key)
    records.append(record)

if not records:
    raise SystemExit("extension benchmark JSONL contains no records")
if observed_coverage != expected_coverage:
    raise SystemExit(
        "extension benchmark coverage mismatch: "
        f"missing={sorted(expected_coverage - observed_coverage)!r}, "
        f"unexpected={sorted(observed_coverage - expected_coverage)!r}"
    )
PY
}

validate_retrieved_ext_bench_harness_pair() {
  local jsonl_path="$1"
  local report_path="$2"
  local expected_mode="$3"
  local manifest_path="$4"
  local expected_commit="$5"
  python3 - "$jsonl_path" "$report_path" "$expected_mode" "$manifest_path" "$expected_commit" <<'PY'
import hashlib
import json
import math
import sys
from datetime import datetime
from pathlib import Path

jsonl_path = Path(sys.argv[1])
report_path = Path(sys.argv[2])
expected_mode = sys.argv[3]
manifest_path = Path(sys.argv[4])
expected_commit = sys.argv[5]
expected_config = {
    "pr": {
        "max_extensions": 10,
        "iterations": 10,
        "event_dispatch_count": 50,
    },
    "nightly": {
        "max_extensions": 200,
        "iterations": 100,
        "event_dispatch_count": 200,
    },
}[expected_mode]

try:
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    report = json.loads(report_path.read_text(encoding="utf-8"))
except (OSError, json.JSONDecodeError) as error:
    raise SystemExit(f"invalid extension benchmark harness input: {error}") from error

if not isinstance(manifest, dict) or manifest.get("schema") != "pi.ext.validated-manifest.v1":
    raise SystemExit("extension benchmark manifest schema mismatch")
manifest_entries = manifest.get("extensions")
if not isinstance(manifest_entries, list) or not manifest_entries:
    raise SystemExit("extension benchmark manifest contains no extensions")
safe_entries = []
all_ids = set()
for index, entry in enumerate(manifest_entries):
    if not isinstance(entry, dict):
        raise SystemExit(f"extension benchmark manifest entry {index} must be an object")
    extension_id = entry.get("id")
    if not isinstance(extension_id, str) or not extension_id.strip():
        raise SystemExit(f"extension benchmark manifest entry {index} has no id")
    if extension_id in all_ids:
        raise SystemExit(f"extension benchmark manifest has duplicate id {extension_id!r}")
    all_ids.add(extension_id)
    capabilities = entry.get("capabilities")
    if not isinstance(capabilities, dict):
        raise SystemExit(f"extension benchmark manifest entry {extension_id!r} has no capabilities")
    for field in (
        "is_multi_file",
        "uses_exec",
        "registers_tools",
        "registers_commands",
        "registers_flags",
        "registers_providers",
    ):
        if type(capabilities.get(field)) is not bool:
            raise SystemExit(
                f"extension benchmark manifest entry {extension_id!r} capabilities.{field} must be boolean"
            )
    subscribed_events = capabilities.get("subscribes_events")
    if not isinstance(subscribed_events, list) or any(
        not isinstance(event, str) for event in subscribed_events
    ):
        raise SystemExit(
            f"extension benchmark manifest entry {extension_id!r} capabilities.subscribes_events must be an array of strings"
        )
    source_tier = entry.get("source_tier")
    conformance_tier = entry.get("conformance_tier")
    if not isinstance(source_tier, str) or not source_tier:
        raise SystemExit(
            f"extension benchmark manifest entry {extension_id!r} source_tier must be a non-empty string"
        )
    if type(conformance_tier) is not int or conformance_tier < 0:
        raise SystemExit(
            f"extension benchmark manifest entry {extension_id!r} conformance_tier must be a non-negative integer"
        )
    if not capabilities["is_multi_file"] and not capabilities["uses_exec"]:
        safe_entries.append(
            {
                "id": extension_id,
                "source_tier": source_tier,
                "conformance_tier": conformance_tier,
                "registers_tools": capabilities["registers_tools"],
                "registers_commands": capabilities["registers_commands"],
                "registers_flags": capabilities["registers_flags"],
                "subscribes_events": subscribed_events,
            }
        )

if not safe_entries:
    raise SystemExit("extension benchmark manifest contains no safe extensions")

records = []
try:
    lines = jsonl_path.read_text(encoding="utf-8").splitlines()
except OSError as error:
    raise SystemExit(f"cannot read extension benchmark JSONL: {error}") from error
for line_number, line in enumerate(lines, start=1):
    if not line.strip():
        continue
    try:
        record = json.loads(line)
    except json.JSONDecodeError as error:
        raise SystemExit(f"extension benchmark JSONL line {line_number}: {error}") from error
    if not isinstance(record, dict):
        raise SystemExit(f"extension benchmark JSONL line {line_number} must be an object")
    records.append(record)

if not records:
    raise SystemExit("extension benchmark JSONL contains no records")

if not isinstance(report, dict):
    raise SystemExit("extension benchmark harness report must be an object")
if report.get("schema") != "pi.bench.harness_report.v1":
    raise SystemExit("extension benchmark harness report schema mismatch")
if report.get("mode") != expected_mode:
    raise SystemExit(
        "extension benchmark harness mode mismatch: "
        f"expected={expected_mode!r} observed={report.get('mode')!r}"
    )
config = report.get("config")
if not isinstance(config, dict):
    raise SystemExit("extension benchmark harness report has no config object")
for field, expected in expected_config.items():
    observed = config.get(field)
    if type(observed) is not int or observed != expected:
        raise SystemExit(
            f"extension benchmark harness config.{field} mismatch: "
            f"expected={expected!r} observed={observed!r}"
        )
if config.get("debug_build") is not False:
    raise SystemExit("extension benchmark harness config.debug_build must equal false")

if expected_mode == "pr":
    selected_entries = []
    selected_ids = set()

    def pick(predicate):
        if len(selected_entries) >= expected_config["max_extensions"]:
            return
        selected = next(
            (
                entry
                for entry in safe_entries
                if entry["id"] not in selected_ids and predicate(entry)
            ),
            None,
        )
        if selected is not None:
            selected_entries.append(selected)
            selected_ids.add(selected["id"])

    pick(lambda entry: entry["source_tier"] == "official-pi-mono" and entry["registers_tools"])
    pick(
        lambda entry: entry["source_tier"] == "official-pi-mono"
        and "agent_start" in entry["subscribes_events"]
    )
    pick(
        lambda entry: entry["source_tier"] == "community"
        and entry["registers_commands"]
        and "agent_start" in entry["subscribes_events"]
    )
    pick(
        lambda entry: entry["source_tier"] == "community"
        and entry["registers_tools"]
        and entry["registers_flags"]
    )
    pick(
        lambda entry: entry["source_tier"] == "npm-registry"
        and entry["registers_commands"]
    )
    pick(
        lambda entry: entry["source_tier"] == "npm-registry"
        and "agent_start" in entry["subscribes_events"]
    )
    for entry in safe_entries:
        if len(selected_entries) >= expected_config["max_extensions"]:
            break
        if entry["id"] not in selected_ids:
            selected_entries.append(entry)
            selected_ids.add(entry["id"])
else:
    selected_entries = safe_entries[: expected_config["max_extensions"]]

expected_ids = [entry["id"] for entry in selected_entries]
expected_entry_by_id = {entry["id"]: entry for entry in selected_entries}
if not any("agent_start" in entry["subscribes_events"] for entry in selected_entries):
    raise SystemExit(
        "extension benchmark selection contains no agent_start subscriber"
    )
if expected_mode == "nightly" and len(safe_entries) > expected_config["max_extensions"]:
    raise SystemExit(
        "nightly extension benchmark max_extensions truncates the safe manifest corpus"
    )

expected_env_fields = {
    "os",
    "arch",
    "cpu_model",
    "cpu_cores",
    "mem_total_mb",
    "build_profile",
    "git_commit",
    "features",
    "config_hash",
}
expected_features = [
    "bpe-tokens",
    "ext-conformance",
    "ftui",
    "sqlite-sessions",
    "tui",
]


def validated_environment(value, location):
    if not isinstance(value, dict) or set(value) != expected_env_fields:
        raise SystemExit(f"{location} environment fields mismatch")
    for field in ("os", "arch", "cpu_model", "build_profile", "git_commit"):
        if not isinstance(value.get(field), str) or not value[field].strip():
            raise SystemExit(f"{location} env.{field} must be a non-empty string")
    if type(value.get("cpu_cores")) is not int or value["cpu_cores"] <= 0:
        raise SystemExit(f"{location} env.cpu_cores must be a positive integer")
    if type(value.get("mem_total_mb")) is not int or value["mem_total_mb"] <= 0:
        raise SystemExit(f"{location} env.mem_total_mb must be a positive integer")
    if value.get("build_profile") != "perf":
        raise SystemExit(f"{location} env.build_profile must equal 'perf'")
    if value.get("git_commit") != expected_commit:
        raise SystemExit(f"{location} env.git_commit must equal the source commit")
    if value.get("features") != expected_features:
        raise SystemExit(
            f"{location} env.features must equal {expected_features!r}"
        )
    hash_input = "|".join(
        str(value[field])
        for field in (
            "os",
            "arch",
            "cpu_model",
            "cpu_cores",
            "mem_total_mb",
            "build_profile",
            "git_commit",
        )
    ) + "|" + ",".join(value["features"])
    expected_hash = hashlib.sha256(hash_input.encode()).hexdigest()
    if value.get("config_hash") != expected_hash:
        raise SystemExit(f"{location} env.config_hash mismatch")
    return value


generated_at = report.get("generated_at")
if not isinstance(generated_at, str) or not generated_at.strip():
    raise SystemExit("extension benchmark harness report generated_at is missing")
try:
    parsed_generated_at = datetime.fromisoformat(generated_at.replace("Z", "+00:00"))
except ValueError as error:
    raise SystemExit("extension benchmark harness report generated_at is invalid") from error
if parsed_generated_at.tzinfo is None:
    raise SystemExit("extension benchmark harness report generated_at must be timezone-aware")
report_env = validated_environment(report.get("env"), "extension benchmark report")

observed = {}
observed_env = None
for index, record in enumerate(records):
    if record.get("schema") != "pi.ext.rust_bench.v1":
        raise SystemExit(f"extension benchmark record {index} schema mismatch")
    if record.get("runtime") != "pi_agent_rust" or record.get("success") is not True:
        raise SystemExit(f"extension benchmark record {index} is not a successful Pi Rust result")
    record_env = validated_environment(
        record.get("env"), f"extension benchmark record {index}"
    )
    if observed_env is None:
        observed_env = record_env
    elif record_env != observed_env:
        raise SystemExit("extension benchmark records use mixed environments")
    scenario = record.get("scenario")
    extension_id = record.get("extension")
    if not isinstance(scenario, str) or not isinstance(extension_id, str):
        raise SystemExit(f"extension benchmark record {index} has invalid coverage identity")
    key = (extension_id, scenario)
    if key in observed:
        raise SystemExit(f"duplicate extension benchmark coverage {key!r}")
    stats = record.get("stats")
    stat_fields = ("count", "min_us", "max_us", "mean_us", "p50_us", "p95_us", "p99_us")
    if not isinstance(stats, dict) or any(
        type(stats.get(field)) is not int or stats[field] < 0 for field in stat_fields
    ):
        raise SystemExit(
            f"extension benchmark record {index} stats must contain non-negative integer fields"
        )
    expected_count = (
        expected_config["event_dispatch_count"]
        if scenario == "event_dispatch"
        else expected_config["iterations"]
    )
    if stats["count"] != expected_count:
        raise SystemExit(
            f"extension benchmark record {key!r} stats.count mismatch: "
            f"expected={expected_count} observed={stats['count']!r}"
        )
    if stats["count"] <= 0 or not (
        stats["min_us"]
        <= stats["p50_us"]
        <= stats["p95_us"]
        <= stats["p99_us"]
        <= stats["max_us"]
        and stats["min_us"] <= stats["mean_us"] <= stats["max_us"]
    ):
        raise SystemExit(f"extension benchmark record {index} stats are incoherent")
    if scenario in {"cold_load", "warm_load"}:
        entry = expected_entry_by_id.get(extension_id)
        if entry is None:
            raise SystemExit(
                f"extension benchmark record {index} uses unselected extension {extension_id!r}"
            )
        expected_group = (
            "official-simple"
            if entry["source_tier"] == "official-pi-mono"
            and entry["conformance_tier"] <= 3
            else (
                "official-complex"
                if entry["source_tier"] == "official-pi-mono"
                else "community"
            )
        )
        if record.get("group") != expected_group or record.get("tier") != entry["conformance_tier"]:
            raise SystemExit(
                f"extension benchmark record {index} group/tier differs from manifest"
            )
    elif scenario == "event_dispatch":
        if record.get("group") != "aggregate" or record.get("tier") != 0:
            raise SystemExit(
                f"extension benchmark record {index} aggregate group/tier mismatch"
            )
    observed[key] = record

expected_coverage = {
    (extension_id, scenario)
    for extension_id in expected_ids
    for scenario in ("cold_load", "warm_load")
}
expected_coverage.add((f"{len(expected_ids)}_extensions", "event_dispatch"))
observed_coverage = set(observed)
if observed_coverage != expected_coverage:
    raise SystemExit(
        "extension benchmark coverage mismatch: "
        f"missing={sorted(expected_coverage - observed_coverage)!r}, "
        f"unexpected={sorted(observed_coverage - expected_coverage)!r}"
    )
if observed_env != report_env:
    raise SystemExit("extension benchmark report environment differs from JSONL")

summary = report.get("summary")
if not isinstance(summary, dict):
    raise SystemExit("extension benchmark harness report has no summary object")
for field in ("total_scenarios", "total_passed", "total_failed"):
    value = summary.get(field)
    if type(value) is not int or value < 0:
        raise SystemExit(
            f"extension benchmark harness summary.{field} must be a non-negative integer"
        )
if summary["total_scenarios"] != len(records):
    raise SystemExit("extension benchmark harness summary total does not match JSONL")
if summary["total_passed"] + summary["total_failed"] != summary["total_scenarios"]:
    raise SystemExit("extension benchmark harness summary totals are inconsistent")
if summary["total_failed"] != 0:
    raise SystemExit("extension benchmark harness report contains failed scenarios")
for field in ("budgets_passed", "budgets_failed", "budgets_no_data"):
    value = summary.get(field)
    if type(value) is not int or value < 0:
        raise SystemExit(
            f"extension benchmark harness summary.{field} must be a non-negative integer"
        )
if summary["budgets_passed"] != 5 or summary["budgets_failed"] != 0 or summary["budgets_no_data"] != 0:
    raise SystemExit("extension benchmark harness report has failed or missing budget data")


def percentile(values, percentile_value):
    if not values:
        return None
    sorted_values = sorted(values)
    raw_index = (percentile_value / 100.0) * (len(sorted_values) - 1)
    index = int(math.floor(raw_index + 0.5))
    return sorted_values[min(index, len(sorted_values) - 1)]


expected_scenario_counts = {
    "cold_load": len(expected_ids),
    "warm_load": len(expected_ids),
    "event_dispatch": 1,
}
by_scenario = report.get("by_scenario")
if not isinstance(by_scenario, dict) or set(by_scenario) != set(expected_scenario_counts):
    raise SystemExit("extension benchmark harness by_scenario coverage mismatch")
for scenario, expected_count in expected_scenario_counts.items():
    row = by_scenario.get(scenario)
    if not isinstance(row, dict):
        raise SystemExit(f"extension benchmark harness by_scenario.{scenario} is invalid")
    if (
        row.get("scenario") != scenario
        or row.get("extensions_tested") != expected_count
        or row.get("passed") != expected_count
        or row.get("failed") != 0
    ):
        raise SystemExit(f"extension benchmark harness by_scenario.{scenario} totals mismatch")
    aggregate_stats = row.get("aggregate_stats")
    representative_p50s = [
        record["stats"]["p50_us"]
        for record in records
        if record.get("scenario") == scenario and record.get("success") is True
    ]
    expected_aggregate_stats = {
        "count": len(representative_p50s),
        "min_us": min(representative_p50s, default=0),
        "max_us": max(representative_p50s, default=0),
        "mean_us": (
            sum(representative_p50s) // len(representative_p50s)
            if representative_p50s
            else 0
        ),
        "p50_us": percentile(representative_p50s, 50.0) or 0,
        "p95_us": percentile(representative_p50s, 95.0) or 0,
        "p99_us": percentile(representative_p50s, 99.0) or 0,
    }
    if (
        not isinstance(aggregate_stats, dict)
        or any(
            type(aggregate_stats.get(field)) is not int
            or aggregate_stats[field] < 0
            for field in (
                "count",
                "min_us",
                "max_us",
                "mean_us",
                "p50_us",
                "p95_us",
                "p99_us",
            )
        )
        or aggregate_stats != expected_aggregate_stats
    ):
        raise SystemExit(
            f"extension benchmark harness by_scenario.{scenario}.aggregate_stats is invalid"
        )

report_results = report.get("results")
if not isinstance(report_results, list) or len(report_results) != len(records):
    raise SystemExit("extension benchmark harness report results do not match JSONL count")
report_by_key = {}
for index, result in enumerate(report_results):
    if not isinstance(result, dict):
        raise SystemExit(f"extension benchmark harness report result {index} is invalid")
    key = (result.get("extension"), result.get("scenario"))
    if key in report_by_key:
        raise SystemExit(f"duplicate extension benchmark report result {key!r}")
    report_by_key[key] = result
if set(report_by_key) != observed_coverage:
    raise SystemExit("extension benchmark harness report result coverage differs from JSONL")
for key, record in observed.items():
    result = report_by_key[key]
    comparable_fields = (
        "schema",
        "runtime",
        "scenario",
        "extension",
        "group",
        "tier",
        "success",
        "error",
        "stats",
        "env",
    )
    if any(result.get(field) != record.get(field) for field in comparable_fields):
        raise SystemExit(f"extension benchmark harness report result {key!r} differs from JSONL")

expected_budget_names = {
    "ext_cold_load_simple_p95",
    "ext_cold_load_per_ext_p99",
    "ext_warm_load_per_ext_p99",
    "event_dispatch_p99",
    "ext_warm_load_p95",
}
budget_checks = report.get("budget_checks")
if not isinstance(budget_checks, list) or len(budget_checks) != len(expected_budget_names):
    raise SystemExit("extension benchmark harness report budget check count mismatch")
budget_check_by_name = {}
for check in budget_checks:
    if not isinstance(check, dict):
        raise SystemExit("extension benchmark harness report contains an invalid budget check")
    budget_name = check.get("budget_name")
    if budget_name in budget_check_by_name:
        raise SystemExit("extension benchmark harness report has duplicate budget checks")
    budget_check_by_name[budget_name] = check
if set(budget_check_by_name) != expected_budget_names:
    raise SystemExit("extension benchmark harness report budget check identities mismatch")

def last_max_record(records_for_budget, stat_field):
    worst = None
    for record in records_for_budget:
        candidate = (record["stats"][stat_field], record["extension"])
        if worst is None or candidate[0] >= worst[0]:
            worst = candidate
    return worst


cold_records = [
    record for record in records if record.get("scenario") == "cold_load"
]
warm_records = [
    record for record in records if record.get("scenario") == "warm_load"
]
event_records = [
    record for record in records if record.get("scenario") == "event_dispatch"
]
cold_simple_p95 = percentile(
    [
        record["stats"]["p95_us"]
        for record in cold_records
        if record.get("group") == "official-simple"
    ],
    95.0,
)
worst_cold_p99 = last_max_record(cold_records, "p99_us")
worst_warm_p99 = last_max_record(warm_records, "p99_us")
event_p99 = event_records[0]["stats"]["p99_us"] if len(event_records) == 1 else None
warm_aggregate_p95 = percentile(
    [record["stats"]["p95_us"] for record in warm_records], 95.0
)
expected_budget_checks = {
    "ext_cold_load_simple_p95": {
        "threshold_us": 200_000,
        "actual_us": cold_simple_p95,
        "worst_extension": None,
    },
    "ext_cold_load_per_ext_p99": {
        "threshold_us": 100_000,
        "actual_us": worst_cold_p99[0] if worst_cold_p99 else None,
        "worst_extension": worst_cold_p99[1] if worst_cold_p99 else None,
    },
    "ext_warm_load_per_ext_p99": {
        "threshold_us": 100_000,
        "actual_us": worst_warm_p99[0] if worst_warm_p99 else None,
        "worst_extension": worst_warm_p99[1] if worst_warm_p99 else None,
    },
    "event_dispatch_p99": {
        "threshold_us": 5_000,
        "actual_us": event_p99,
        "worst_extension": None,
    },
    "ext_warm_load_p95": {
        "threshold_us": 100_000,
        "actual_us": warm_aggregate_p95,
        "worst_extension": None,
    },
}
for budget_name, expected in expected_budget_checks.items():
    expected_status = (
        "NO_DATA"
        if expected["actual_us"] is None
        else (
            "PASS"
            if expected["actual_us"] <= expected["threshold_us"]
            else "FAIL"
        )
    )
    observed_check = budget_check_by_name[budget_name]
    if (
        observed_check.get("threshold_us") != expected["threshold_us"]
        or observed_check.get("actual_us") != expected["actual_us"]
        or observed_check.get("status") != expected_status
        or observed_check.get("worst_extension") != expected["worst_extension"]
    ):
        raise SystemExit(
            f"extension benchmark harness budget {budget_name!r} differs from recomputed JSONL evidence"
        )
PY
}

validate_retrieved_rust_bench_jsonl() {
  local artifact_path="$1"
  local producer_kind="$2"
  local expected_commit="$3"
  local expected_correlation_id="$4"
  python3 - \
    "$artifact_path" \
    "$producer_kind" \
    "$expected_commit" \
    "$expected_correlation_id" <<'PY'
import json
import sys
from pathlib import Path

artifact_path = Path(sys.argv[1])
producer_kind = sys.argv[2]
expected_commit = sys.argv[3]
expected_correlation_id = sys.argv[4]
required_scenarios = {
    "scenario": {
        "cold_start",
        "warm_start",
        "tool_call",
        "event_dispatch",
        "session_workload_matrix",
    },
    "extension": {"cold_load", "warm_load", "event_dispatch"},
}[producer_kind]
observed_scenarios = set()
record_count = 0

for line_number, line in enumerate(
    artifact_path.read_text(encoding="utf-8").splitlines(), start=1
):
    if not line.strip():
        continue
    try:
        record = json.loads(line)
    except json.JSONDecodeError as error:
        raise SystemExit(f"line {line_number}: invalid JSON: {error}") from error
    if not isinstance(record, dict):
        raise SystemExit(f"line {line_number}: benchmark record must be an object")
    if record.get("schema") != "pi.ext.rust_bench.v1":
        raise SystemExit(f"line {line_number}: benchmark schema mismatch")
    if record.get("source_commit") != expected_commit:
        raise SystemExit(f"line {line_number}: source_commit mismatch")
    if record.get("source_dirty") is not False:
        raise SystemExit(f"line {line_number}: source_dirty must equal false")
    if record.get("run_id") != expected_correlation_id:
        raise SystemExit(f"line {line_number}: run_id mismatch")
    timestamp = record.get("timestamp")
    if not isinstance(timestamp, str) or not timestamp.strip():
        raise SystemExit(f"line {line_number}: timestamp is missing")
    if producer_kind == "scenario":
        if record.get("runtime") != "pi_agent_rust":
            raise SystemExit(f"line {line_number}: runtime mismatch")
        if record.get("orchestration_correlation_id") != expected_correlation_id:
            raise SystemExit(
                f"line {line_number}: orchestration_correlation_id mismatch"
            )
    else:
        if record.get("correlation_id") != expected_correlation_id:
            raise SystemExit(f"line {line_number}: correlation_id mismatch")
        if record.get("success") is not True:
            raise SystemExit(f"line {line_number}: extension scenario did not succeed")
    scenario = record.get("scenario")
    if not isinstance(scenario, str) or not scenario:
        raise SystemExit(f"line {line_number}: scenario is missing")
    observed_scenarios.add(scenario)
    record_count += 1

if record_count == 0:
    raise SystemExit("benchmark JSONL contains no records")
missing_scenarios = required_scenarios - observed_scenarios
if missing_scenarios:
    raise SystemExit(
        f"benchmark JSONL is missing required scenarios: {sorted(missing_scenarios)!r}"
    )
PY
}

validate_retrieved_legacy_bench_jsonl() {
  local artifact_path="$1"
  local expected_commit="$2"
  local expected_correlation_id="$3"
  python3 - \
    "$artifact_path" \
    "$expected_commit" \
    "$expected_correlation_id" <<'PY'
import json
import math
import sys
from pathlib import Path

artifact_path = Path(sys.argv[1])
expected_commit = sys.argv[2]
expected_correlation_id = sys.argv[3]
required = {
    (runtime_kind, scenario, extension)
    for runtime_kind in ("node", "bun")
    for scenario, extension in (
        ("ext_load_init/load_init_cold", "hello"),
        ("ext_load_init/load_init_cold", "pirate"),
        ("ext_tool_call/hello", "hello"),
        ("ext_event_hook/before_agent_start", "pirate"),
        ("full_e2e_long_session", "hello+pirate"),
    )
}
observed = set()

for line_number, line in enumerate(
    artifact_path.read_text(encoding="utf-8").splitlines(), start=1
):
    if not line.strip():
        continue
    try:
        record = json.loads(line)
    except json.JSONDecodeError as error:
        raise SystemExit(f"line {line_number}: invalid JSON: {error}") from error
    if not isinstance(record, dict):
        raise SystemExit(f"line {line_number}: legacy benchmark record must be an object")
    if record.get("schema") != "pi.ext.legacy_bench.v1":
        raise SystemExit(f"line {line_number}: legacy benchmark schema mismatch")
    if record.get("source_commit") != expected_commit:
        raise SystemExit(f"line {line_number}: source_commit mismatch")
    if record.get("source_dirty") is not False:
        raise SystemExit(f"line {line_number}: source_dirty must equal false")
    for field in ("run_id", "correlation_id"):
        if record.get(field) != expected_correlation_id:
            raise SystemExit(f"line {line_number}: {field} mismatch")
    timestamp = record.get("timestamp")
    if not isinstance(timestamp, str) or not timestamp.strip():
        raise SystemExit(f"line {line_number}: timestamp is missing")
    runtime_kind = record.get("runtime_kind")
    scenario = record.get("scenario")
    extension = record.get("extension")
    if runtime_kind not in {"node", "bun"} or not isinstance(scenario, str):
        raise SystemExit(f"line {line_number}: runtime/scenario contract mismatch")
    legacy_pi_mono_executed = record.get("legacy_pi_mono_executed") is True
    if legacy_pi_mono_executed:
        if (
            record.get("runtime") != "legacy_pi_mono"
            or record.get("runtime_family") != "legacy_pi_mono_extension_loader"
        ):
            raise SystemExit(
                f"line {line_number}: true pi-mono evidence has invalid runtime identity"
            )
    elif (
        record.get("runtime") != f"portable_{runtime_kind}_extension_api"
        or record.get("runtime_family") != "portable_extension_api"
    ):
        raise SystemExit(
            f"line {line_number}: portable shim evidence must identify its runtime honestly"
        )
    key = (runtime_kind, scenario, extension)
    if key not in required:
        raise SystemExit(f"line {line_number}: unexpected legacy benchmark row {key!r}")
    if key in observed:
        raise SystemExit(f"line {line_number}: duplicate required legacy row {key!r}")
    if scenario == "ext_load_init/load_init_cold":
        summary = record.get("summary")
        if (
            not isinstance(summary, dict)
            or record.get("runs") != 10
            or summary.get("count") != 10
        ):
            raise SystemExit(f"line {line_number}: cold-load sampling contract mismatch")
        values = (summary.get("p50_ms"), summary.get("p95_ms"))
    elif scenario in {"ext_tool_call/hello", "ext_event_hook/before_agent_start"}:
        if record.get("iterations") != 2000:
            raise SystemExit(f"line {line_number}: dispatch iteration contract mismatch")
        values = (record.get("per_call_us"),)
    else:
        expected_shape_fields = {
            "extension_loads_per_iteration": 2,
            "tool_calls_per_iteration": 10,
            "event_hooks_per_iteration": 1,
        }
        workload_shape = record.get("workload_shape")
        if (
            record.get("iterations") != 2000
            or record.get("tool_calls_per_iteration") != 10
            or record.get("tool_executions") != 20000
            or record.get("event_executions") != 2000
            or not isinstance(workload_shape, dict)
            or set(workload_shape) != {*expected_shape_fields, "description"}
            or any(
                workload_shape.get(field) != expected
                for field, expected in expected_shape_fields.items()
            )
            or not isinstance(workload_shape.get("description"), str)
            or not workload_shape["description"].strip()
        ):
            raise SystemExit(f"line {line_number}: full-session workload shape mismatch")
        values = (record.get("elapsed_ms"),)
    if any(
        isinstance(value, bool)
        or not isinstance(value, (int, float))
        or not math.isfinite(value)
        or value <= 0
        for value in values
    ):
        raise SystemExit(f"line {line_number}: required legacy metric is invalid")
    observed.add(key)

if observed != required:
    raise SystemExit(
        "legacy benchmark coverage mismatch: "
        f"missing={sorted(required - observed)!r}"
    )
PY
}

run_test_suite() {
  local suite_name="$1"
  local target_name="$2"
  local suite_start suite_end suite_elapsed exit_code
  local rch_target_subdir=""
  local benchmark_run_id=""
  local remote_execution_required=false
  local remote_execution_verified=false
  case "${RCH_REQUIRE_REMOTE:-0}" in
    1|true|TRUE|yes|YES|on|ON) remote_execution_required=true ;;
  esac

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
    elif [[ -e "$result_dir/extension_bench.jsonl" \
      || -L "$result_dir/extension_bench.jsonl" \
      || -e "$result_dir/extension_bench_summary.md" \
      || -L "$result_dir/extension_bench_summary.md" ]]; then
      log_fail "Refusing preexisting accepted extension benchmark artifacts"
      exit_code=95
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
        "$benchmark_run_id" \
        "$PROFILE"; then
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
            "$benchmark_run_id" \
            "$PROFILE"; then
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
        if [[ -e "$result_dir/extension_bench_summary.md" \
          || -L "$result_dir/extension_bench_summary.md" ]]; then
          log_fail "Refusing preexisting accepted extension benchmark summary"
          exit_code=97
        else
          cp "$retrieved_result_dir/extension_bench_summary.md" "$result_dir/extension_bench_summary.md"
        fi
      fi
    elif [[ "$exit_code" -eq 0 ]]; then
      log_fail "RCH completed $suite_name without retrieving extension_bench.jsonl from $rch_target_subdir"
      exit_code=86
    fi
    if [[ "$exit_code" -eq 0 ]]; then
      remote_execution_verified=true
    fi
  elif [[ "$CARGO_RUNNER_MODE" == "rch" \
    && ( "$suite_name" == "bench_scenario" \
      || "$suite_name" == "ext_bench_harness" ) ]]; then
    rch_target_subdir="nextest/pi-perf/$RUN_INSTANCE_ID/$suite_name"
    local retrieved_result_dir="$TARGET_DIR/$rch_target_subdir"
    local -a returned_artifacts=()
    local -a producer_feature_args=()
    local producer_bench_mode="pr"
    local producer_legacy_runtimes=0
    if [[ "$PROFILE" == "full" ]]; then
      producer_bench_mode="nightly"
    fi
    case "$suite_name" in
      bench_scenario)
        returned_artifacts=(scenario_runner.jsonl)
        if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
          returned_artifacts+=(legacy_extension_workloads.jsonl)
          producer_legacy_runtimes=1
        fi
        ;;
      ext_bench_harness)
        returned_artifacts=(ext_bench_harness.jsonl ext_bench_harness_report.json)
        producer_feature_args=(--features ext-conformance)
        ;;
    esac
    if ! verify_current_clean_source_identity "RCH $suite_name producer precondition"; then
      exit_code=89
    elif [[ -e "$retrieved_result_dir" || -L "$retrieved_result_dir" ]]; then
      log_fail "Refusing stale RCH producer directory: $retrieved_result_dir"
      exit_code=87
    else
      BENCH_OUTPUT_TARGET_SUBDIR="$rch_target_subdir" \
      CI_CORRELATION_ID="$CORRELATION_ID" \
      VERGEN_GIT_SHA="$GIT_COMMIT_FULL" \
      VERGEN_GIT_DIRTY="$GIT_DIRTY" \
      RUST_TEST_THREADS="$PARALLELISM" \
      CARGO_BUILD_JOBS="$BUILD_JOBS" \
      PI_BENCH_BUILD_PROFILE="$CARGO_PROFILE" \
      PI_BENCH_MODE="$producer_bench_mode" \
      PI_BENCH_LEGACY_RUNTIMES="$producer_legacy_runtimes" \
      RCH_REQUIRE_REMOTE=1 \
      RCH_QUIET=0 \
      RCH_VISIBILITY=summary \
        "${PERF_BENCH_RUNNER_ARGS[@]}" nextest run \
          --build-jobs "$BUILD_JOBS" \
          --test "$target_name" \
          --cargo-profile "$CARGO_PROFILE" \
          --test-threads 1 \
          --no-tests fail \
          "${producer_feature_args[@]}" \
          -- --nocapture \
        >"$result_dir/stdout.log" 2>"$result_dir/stderr.log" \
        || exit_code=$?
    fi
    if [[ "$exit_code" -eq 0 ]] \
      && ! verify_current_clean_source_identity "RCH $suite_name producer postcondition"; then
      exit_code=91
    fi
    if [[ "$exit_code" -eq 0 ]]; then
      if ! grep -Eqs '^[[]RCH[]] remote [^[:space:]]+ [(][^)]+[)]$' \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "$suite_name producer has no remote-success marker"
        exit_code=92
      elif grep -Eqs '^[[]RCH[]] local( |$)' \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "$suite_name producer reported local execution"
        exit_code=93
      elif ! grep -Eqs \
        "^[[]RCH[]] clean-overlay receipt: base=$GIT_COMMIT_FULL overlay-fingerprint=[0-9a-f]{64}$" \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "$suite_name producer has no current-commit clean-overlay receipt"
        exit_code=94
      fi
    fi
    if [[ "$exit_code" -eq 0 ]]; then
      local artifact_name source_path accepted_path
      for artifact_name in "${returned_artifacts[@]}"; do
        source_path="$retrieved_result_dir/$artifact_name"
        if [[ ! -s "$source_path" || -L "$source_path" ]]; then
          log_fail "$suite_name did not return regular nonempty $artifact_name"
          exit_code=86
          break
        fi
      done
    fi
    if [[ "$exit_code" -eq 0 ]]; then
      for artifact_name in "${returned_artifacts[@]}"; do
        source_path="$retrieved_result_dir/$artifact_name"
        if [[ "$artifact_name" == "scenario_runner.jsonl" ]] \
          && ! validate_retrieved_rust_bench_jsonl \
            "$source_path" scenario "$GIT_COMMIT_FULL" "$CORRELATION_ID"; then
          log_fail "$suite_name returned invalid scenario benchmark evidence"
          exit_code=88
          break
        fi
        if [[ "$artifact_name" == "ext_bench_harness.jsonl" ]] \
          && { ! validate_retrieved_rust_bench_jsonl \
                 "$source_path" extension "$GIT_COMMIT_FULL" "$CORRELATION_ID" \
               || ! validate_retrieved_ext_bench_harness_pair \
                 "$source_path" \
                 "$retrieved_result_dir/ext_bench_harness_report.json" \
                 "$producer_bench_mode" \
                 "$PROJECT_ROOT/tests/ext_conformance/VALIDATED_MANIFEST.json" \
                 "$GIT_COMMIT_FULL"; }; then
          log_fail "$suite_name returned invalid extension benchmark evidence"
          exit_code=88
          break
        fi
        if [[ "$artifact_name" == "legacy_extension_workloads.jsonl" ]] \
          && ! validate_retrieved_legacy_bench_jsonl \
            "$source_path" "$GIT_COMMIT_FULL" "$CORRELATION_ID"; then
          log_fail "$suite_name returned invalid Node+Bun legacy benchmark evidence"
          exit_code=88
          break
        fi
      done
    fi
    if [[ "$exit_code" -eq 0 ]]; then
      for artifact_name in "${returned_artifacts[@]}"; do
        source_path="$retrieved_result_dir/$artifact_name"
        accepted_path="$OUTPUT_DIR/results/$artifact_name"
        if [[ -e "$accepted_path" || -L "$accepted_path" ]]; then
          log_fail "Refusing preexisting accepted $artifact_name"
          exit_code=95
          break
        fi
        cp "$source_path" "$accepted_path"
        if ! cmp -s "$source_path" "$accepted_path"; then
          log_fail "Accepted $artifact_name copy differs from the RCH return"
          exit_code=96
          break
        fi
      done
    fi
    if [[ "$exit_code" -eq 0 ]]; then
      remote_execution_verified=true
    fi
  else
    local -a suite_runner_args=("${CARGO_RUNNER_ARGS[@]}")
    if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
      suite_runner_args=("${PERF_BENCH_RUNNER_ARGS[@]}")
    fi
    local -a controller_output_env=()
    if [[ "$CARGO_RUNNER_MODE" != "rch" ]]; then
      controller_output_env=(
        "BENCH_OUTPUT_DIR=$result_dir"
        "PERF_REGRESSION_OUTPUT=$result_dir"
        "PERF_RELEASE_BINARY_PATH=$TARGET_DIR/release/pi"
      )
    fi
    env \
      "${controller_output_env[@]}" \
      CI_CORRELATION_ID="$CORRELATION_ID" \
      VERGEN_GIT_SHA="$GIT_COMMIT_FULL" \
      VERGEN_GIT_DIRTY="$GIT_DIRTY" \
      RUST_TEST_THREADS="$PARALLELISM" \
      RCH_REQUIRE_REMOTE="${RCH_REQUIRE_REMOTE:-0}" \
      RCH_QUIET=0 \
      RCH_VISIBILITY=summary \
        "${suite_runner_args[@]}" test --test "$target_name" --profile "$CARGO_PROFILE" -- --nocapture \
        >"$result_dir/stdout.log" 2>"$result_dir/stderr.log" \
        || exit_code=$?
    if [[ "$exit_code" -eq 0 && "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
      if ! verify_current_clean_source_identity "RCH $suite_name postcondition"; then
        exit_code=91
      elif ! grep -Eqs '^[[]RCH[]] remote [^[:space:]]+ [(][^)]+[)]$' \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "$suite_name has no remote-success marker"
        exit_code=92
      elif grep -Eqs '^[[]RCH[]] local( |$)' \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "$suite_name reported local execution"
        exit_code=93
      elif ! grep -Eqs \
        "^[[]RCH[]] clean-overlay receipt: base=$GIT_COMMIT_FULL overlay-fingerprint=[0-9a-f]{64}$" \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
        log_fail "$suite_name has no current-commit clean-overlay receipt"
        exit_code=94
      else
        remote_execution_verified=true
      fi
    fi
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
  "kind": "cargo_test",
  "status": "$status",
  "exit_code": $exit_code,
  "elapsed_ms": $suite_elapsed,
  "correlation_id": "$CORRELATION_ID",
  "run_instance_id": "$RUN_INSTANCE_ID",
  "source_commit": "$GIT_COMMIT_FULL",
  "source_dirty": $GIT_DIRTY,
  "runner_mode": "$CARGO_RUNNER_MODE",
  "remote_execution_required": $remote_execution_required,
  "remote_execution_verified": $remote_execution_verified,
  "timestamp": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "profile": "$CARGO_PROFILE"
}
EOF

  SUITE_RESULTS+=("{\"suite\":\"$suite_name\",\"status\":\"$status\",\"exit_code\":$exit_code,\"elapsed_ms\":$suite_elapsed}")
}

validate_retrieved_pijs_pair() {
  local artifact_path="$1"
  local binary_path="$2"
  local expected_commit="$3"
  local expected_correlation_id="$4"
  python3 - \
    "$artifact_path" \
    "$binary_path" \
    "$expected_commit" \
    "$expected_correlation_id" <<'PY'
import hashlib
import json
import re
import stat
import sys
from pathlib import Path

artifact_path = Path(sys.argv[1])
binary_path = Path(sys.argv[2])
expected_commit = sys.argv[3]
expected_correlation = sys.argv[4]


def stable_regular_bytes(path, executable=False):
    before = path.lstat()
    if stat.S_ISLNK(before.st_mode) or not stat.S_ISREG(before.st_mode):
        raise SystemExit(f"retrieved PiJS artifact is not a regular file: {path}")
    if executable and before.st_mode & 0o111 == 0:
        raise SystemExit(f"retrieved PiJS executable has no execute bit: {path}")
    encoded = path.read_bytes()
    after = path.lstat()
    identity = lambda metadata: (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_size,
        metadata.st_mtime_ns,
    )
    if identity(before) != identity(after) or len(encoded) != after.st_size:
        raise SystemExit(f"retrieved PiJS artifact changed while read: {path}")
    return encoded


binary_bytes = stable_regular_bytes(binary_path, executable=True)
binary_sha256 = hashlib.sha256(binary_bytes).hexdigest()
try:
    artifact_text = stable_regular_bytes(artifact_path).decode("utf-8")
except UnicodeDecodeError as error:
    raise SystemExit(f"retrieved PiJS JSONL is not UTF-8: {error}") from error
lines = artifact_text.splitlines()
if len(lines) != 2 or any(not line.strip() for line in lines):
    raise SystemExit("retrieved PiJS JSONL must contain exactly two nonempty records")
try:
    records = [json.loads(line) for line in lines]
except json.JSONDecodeError as error:
    raise SystemExit(f"retrieved PiJS JSONL is invalid: {error}") from error

expected_features = [
    "clipboard",
    "image",
    "image-resize",
    "sqlite-sessions",
    "tui",
    "wasm-host",
]
observed_tool_calls = set()
claimed_binaries = set()
for index, record in enumerate(records, start=1):
    if not isinstance(record, dict):
        raise SystemExit(f"retrieved PiJS record {index} is not an object")
    expected = {
        "schema": "pi.perf.workload.v1",
        "run_id": expected_correlation,
        "correlation_id": expected_correlation,
        "source_commit": expected_commit,
        "source_dirty": False,
        "tool": "pijs_workload",
        "scenario": "tool_call_roundtrip",
        "iterations": 2000,
        "build_profile": "perf",
        "build_profile_verified": True,
        "build_fingerprint_contract": "cargo_build_fingerprint.v1",
        "build_fingerprint_verified": True,
        "compiled_profile_family": "release",
        "compiled_opt_level": "3",
        "compiled_debug": "true",
        "compiled_features": expected_features,
        "executable_build_profile": "perf",
        "executable_profile_verified": True,
        "debug_assertions": False,
        "runtime_engine": "quickjs",
        "evidence_class": "measured",
        "confidence": "high",
        "eligible_for_regression_gate": True,
        "measurement_method": "wall_clock_observation",
        "measurement_boundary": "production_extension_manager",
        "measurement_contract_version": "production_extension_manager.v1",
        "disk_cache_policy": "disabled",
        "host_page_cache_policy": "not_applicable_measured_region",
        "allocator_requested": "system",
        "allocator_request_source": "env",
        "allocator_effective": "system",
        "allocator_fallback_reason": None,
    }
    mismatches = {
        field: {"expected": value, "observed": record.get(field)}
        for field, value in expected.items()
        if record.get(field) != value
    }
    if mismatches:
        raise SystemExit(f"retrieved PiJS record {index} mismatch: {mismatches}")
    tool_calls = record.get("tool_calls_per_iteration")
    if type(tool_calls) is not int or tool_calls not in {1, 10}:
        raise SystemExit(f"retrieved PiJS record {index} has invalid tool-call count")
    if record.get("total_calls") != 2000 * tool_calls:
        raise SystemExit(f"retrieved PiJS record {index} has invalid total_calls")
    elapsed_us = record.get("elapsed_us")
    if type(elapsed_us) is not int or elapsed_us <= 0:
        raise SystemExit(f"retrieved PiJS record {index} has invalid elapsed_us")
    claimed_path = record.get("binary_path")
    claimed_sha256 = record.get("binary_sha256")
    config_hash = record.get("config_hash")
    claimed_parts = Path(claimed_path).parts if isinstance(claimed_path, str) else ()
    if (
        not isinstance(claimed_path, str)
        or not Path(claimed_path).is_absolute()
        or any(part in {"", ".", ".."} for part in Path(claimed_path).parts)
        or re.fullmatch(r"pijs_workload-[0-9a-f]{16}", Path(claimed_path).name) is None
        or len(claimed_parts) < 3
        or claimed_parts[-3:-1] != ("perf", "deps")
        or not isinstance(claimed_sha256, str)
        or re.fullmatch(r"[0-9a-f]{64}", claimed_sha256) is None
        or not isinstance(config_hash, str)
        or re.fullmatch(r"[0-9a-f]{64}", config_hash) is None
    ):
        raise SystemExit(f"retrieved PiJS record {index} has invalid binary provenance")
    observed_tool_calls.add(tool_calls)
    claimed_binaries.add((claimed_path, claimed_sha256))

if observed_tool_calls != {1, 10} or len(claimed_binaries) != 1:
    raise SystemExit("retrieved PiJS records do not form one exact same-binary 2000x1/2000x10 pair")
_, claimed_sha256 = claimed_binaries.pop()
if claimed_sha256 != binary_sha256:
    raise SystemExit(
        "retrieved PiJS executable digest does not match the two measurement records"
    )
PY
}

run_criterion_bench() {
  local suite_name="$1"
  local bench_name="$2"
  local suite_start suite_end suite_elapsed exit_code

  log_step "Running criterion bench: $suite_name (bench=$bench_name)"
  suite_start=$(epoch_ms)

  local result_dir="$OUTPUT_DIR/results/$suite_name"
  local criterion_run_subdir="pi-perf-runs/$RUN_INSTANCE_ID/$suite_name"
  local criterion_dir="$TARGET_DIR/criterion/$criterion_run_subdir"
  local remote_execution_verified=false
  local -a criterion_runner_args=("${CARGO_RUNNER_ARGS[@]}")
  local -a criterion_cargo_args=(
    bench --bench "$bench_name" --profile "$CARGO_PROFILE"
  )
  if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
    criterion_runner_args=("${PERF_BENCH_RUNNER_ARGS[@]}")
  fi
  if [[ "$suite_name" == "criterion_pijs" ]]; then
    criterion_cargo_args+=(
      --no-default-features
      --features clipboard,image,image-resize,sqlite-sessions,tui,wasm-host
      --
      --regression-gate-pair
    )
  fi
  mkdir -p "$result_dir"

  exit_code=0
  if [[ -e "$criterion_dir" || -L "$criterion_dir" ]]; then
    log_fail "Refusing preexisting Criterion run output: $criterion_dir"
    exit_code=88
  else
    PI_CRITERION_OUTPUT_SUBDIR="$criterion_run_subdir" \
    PI_BENCH_RUN_ID="$CORRELATION_ID" \
    PI_BENCH_CORRELATION_ID="$CORRELATION_ID" \
    PI_BENCH_ALLOCATOR=system \
    PI_BENCH_BUILD_PROFILE="$CARGO_PROFILE" \
    CI_CORRELATION_ID="$CORRELATION_ID" \
    VERGEN_GIT_SHA="$GIT_COMMIT_FULL" \
    VERGEN_GIT_DIRTY="$GIT_DIRTY" \
    RCH_REQUIRE_REMOTE=1 \
    RCH_QUIET=0 \
    RCH_VISIBILITY=summary \
      "${criterion_runner_args[@]}" "${criterion_cargo_args[@]}" \
      >"$result_dir/stdout.log" 2>"$result_dir/stderr.log" \
      || exit_code=$?
  fi

  if [[ "$exit_code" -eq 0 && "$CARGO_RUNNER_MODE" == "rch" ]]; then
    if ! grep -Eqs '^\[RCH\] remote [^[:space:]]+ \([^)]+\)$' \
      "$result_dir/stdout.log" "$result_dir/stderr.log"; then
      log_fail "$suite_name has no remote-success marker"
      exit_code=89
    elif grep -Eqs '^\[RCH\] local( |$)' \
      "$result_dir/stdout.log" "$result_dir/stderr.log"; then
      log_fail "$suite_name reported local execution"
      exit_code=90
    elif [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]] \
      && ! grep -Eqs \
        "^\\[RCH\\] clean-overlay receipt: base=$GIT_COMMIT_FULL overlay-fingerprint=[0-9a-f]{64}$" \
        "$result_dir/stdout.log" "$result_dir/stderr.log"; then
      log_fail "$suite_name has no current-commit clean-overlay receipt"
      exit_code=93
    else
      remote_execution_verified=true
    fi
  fi
  if [[ "$exit_code" -eq 0 && "$CARGO_RUNNER_MODE" == "rch" ]] \
    && ! verify_current_clean_source_identity "RCH $suite_name postcondition"; then
    remote_execution_verified=false
    exit_code=91
  fi
  if [[ "$exit_code" -eq 0 \
    && ( ! -d "$criterion_dir" || -L "$criterion_dir" ) ]]; then
    log_fail "$suite_name did not produce its isolated Criterion directory"
    exit_code=92
  fi
  if [[ "$exit_code" -eq 0 && "$suite_name" == "criterion_pijs" ]]; then
    local retrieved_pijs="$criterion_dir/pijs_workload.jsonl"
    local retrieved_binary="$criterion_dir/pijs_workload"
    local accepted_pijs="$OUTPUT_DIR/results/pijs_workload.jsonl"
    if ! validate_retrieved_pijs_pair \
      "$retrieved_pijs" \
      "$retrieved_binary" \
      "$GIT_COMMIT_FULL" \
      "$CORRELATION_ID"; then
      log_fail "criterion_pijs returned an invalid workload pair or executable"
      exit_code=94
    elif [[ -e "$accepted_pijs" || -L "$accepted_pijs" ]]; then
      log_fail "Refusing preexisting accepted PiJS evidence destination"
      exit_code=95
    else
      cp "$retrieved_pijs" "$accepted_pijs"
      if ! validate_retrieved_pijs_pair \
        "$accepted_pijs" \
        "$retrieved_binary" \
        "$GIT_COMMIT_FULL" \
        "$CORRELATION_ID"; then
        log_fail "Accepted PiJS evidence copy failed post-copy validation"
        exit_code=96
      fi
    fi
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
  "run_instance_id": "$RUN_INSTANCE_ID",
  "source_commit": "$GIT_COMMIT_FULL",
  "source_dirty": $GIT_DIRTY,
  "output_relative": "criterion/$criterion_run_subdir",
  "runner_mode": "$CARGO_RUNNER_MODE",
  "remote_execution_verified": $remote_execution_verified,
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
    if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
      log_step "Deferring perf_budgets to the hermetic post-generation evidence package"
    else
      deferred_perf_budgets=true
    fi
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

if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
  POST_GENERATION_PRODUCER_ADMISSION_PATH="$OUTPUT_DIR/results/post_generation_producer_admission.json"
  if ! OUTPUT_DIR="$OUTPUT_DIR" \
    ADMISSION_PATH="$POST_GENERATION_PRODUCER_ADMISSION_PATH" \
    GIT_COMMIT_FULL="$GIT_COMMIT_FULL" \
    CORRELATION_ID="$CORRELATION_ID" \
    RUN_INSTANCE_ID="$RUN_INSTANCE_ID" \
    CARGO_PROFILE="$CARGO_PROFILE" \
    python3 - <<'PY'
import json
import hashlib
import os
import re
import stat
from datetime import datetime, timezone
from pathlib import Path

output_dir = Path(os.environ["OUTPUT_DIR"])
admission_path = Path(os.environ["ADMISSION_PATH"])
expected_commit = os.environ["GIT_COMMIT_FULL"]
expected_correlation = os.environ["CORRELATION_ID"]
expected_instance = os.environ["RUN_INSTANCE_ID"]
expected_profile = os.environ["CARGO_PROFILE"]
failures = []
admitted_producers = []
admitted_support_checks = []

remote_marker_pattern = re.compile(
    r"^\[RCH\] remote (?P<worker>[^\s]+) \([^)]+\)$"
)
local_marker_pattern = re.compile(r"^\[RCH\] local(?: |$)")
receipt_pattern = re.compile(
    rf"^\[RCH\] clean-overlay receipt: base={re.escape(expected_commit)} "
    r"overlay-fingerprint=(?P<fingerprint>[0-9a-f]{64})$"
)

required_test_producers = {
    "bench_scenario": "bench_scenario_runner",
    "ext_bench_harness": "ext_bench_harness",
    "perf_bench_harness": "perf_bench_harness",
}
required_support_checks = {
    "bench_schema": "bench_schema",
    "perf_regression": "perf_regression",
    "perf_comparison": "perf_comparison",
    "perf_baseline_variance": "perf_baseline_variance",
}
required_criterion_suites = {
    "criterion_extensions": "extensions",
    "criterion_pijs": "pijs_workload",
    "criterion_system": "system",
    "criterion_semantic_context": "semantic_context",
}


def load_current_result(suite):
    path = output_dir / "results" / suite / "result.json"
    try:
        metadata = path.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            raise OSError("result is not a regular file")
        encoded = path.read_bytes()
        return path, json.loads(encoded)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        failures.append(
            {"suite": suite, "reason": "missing_or_invalid_result", "detail": str(error)}
        )
        return path, None


def load_remote_proof(suite):
    lines = []
    for log_name in ("stdout.log", "stderr.log"):
        path = output_dir / "results" / suite / log_name
        try:
            metadata = path.lstat()
            if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
                raise OSError("log is not a regular file")
            lines.extend(path.read_text(encoding="utf-8").splitlines())
        except (OSError, UnicodeDecodeError) as error:
            failures.append(
                {
                    "suite": suite,
                    "reason": "missing_or_invalid_remote_log",
                    "log": log_name,
                    "detail": str(error),
                }
            )
            return None

    remote_markers = [
        match for line in lines if (match := remote_marker_pattern.fullmatch(line))
    ]
    local_markers = [line for line in lines if local_marker_pattern.match(line)]
    receipts = [
        match for line in lines if (match := receipt_pattern.fullmatch(line))
    ]
    if len(remote_markers) != 1 or local_markers or len(receipts) != 1:
        failures.append(
            {
                "suite": suite,
                "reason": "invalid_remote_execution_receipt",
                "remote_marker_count": len(remote_markers),
                "local_marker_count": len(local_markers),
                "clean_overlay_receipt_count": len(receipts),
            }
        )
        return None

    remote_marker = remote_markers[0].group(0)
    remote_worker = remote_markers[0].group("worker")
    clean_overlay_receipt = receipts[0].group(0)
    overlay_fingerprint = receipts[0].group("fingerprint")
    return {
        "remote_marker": remote_marker,
        "remote_worker": remote_worker,
        "clean_overlay_receipt": clean_overlay_receipt,
        "overlay_fingerprint": overlay_fingerprint,
    }


def validate_common(suite, target, kind, result, admitted, extra_expected=None):
    expected = {
        "schema": "pi.perf.suite_result.v1",
        "suite_name": suite,
        "target": target,
        "kind": kind,
        "status": "pass",
        "correlation_id": expected_correlation,
        "run_instance_id": expected_instance,
        "source_commit": expected_commit,
        "source_dirty": False,
        "runner_mode": "rch",
        "profile": expected_profile,
        "remote_execution_verified": True,
    }
    if extra_expected:
        expected.update(extra_expected)
    mismatches = {
        key: {"expected": expected_value, "observed": result.get(key)}
        for key, expected_value in expected.items()
        if result.get(key) != expected_value
    }
    if type(result.get("exit_code")) is not int or result.get("exit_code") != 0:
        mismatches["exit_code"] = {
            "expected": 0,
            "observed": result.get("exit_code"),
        }
    elapsed_ms = result.get("elapsed_ms")
    if type(elapsed_ms) is not int or elapsed_ms < 0:
        mismatches["elapsed_ms"] = {
            "expected": "non-negative integer",
            "observed": elapsed_ms,
        }
    remote_proof = load_remote_proof(suite)
    if mismatches:
        failures.append(
            {"suite": suite, "reason": "producer_result_mismatch", "fields": mismatches}
        )
    if not mismatches and remote_proof is not None:
        producer = {
            "suite": suite,
            "target": target,
            "kind": kind,
            "remote_execution_verified": True,
        }
        producer.update(remote_proof)
        admitted.append(producer)


for suite, target in required_test_producers.items():
    _, result = load_current_result(suite)
    if result is None:
        continue
    validate_common(
        suite,
        target,
        "cargo_test",
        result,
        admitted_producers,
        {"remote_execution_required": True},
    )

for suite, target in required_support_checks.items():
    _, result = load_current_result(suite)
    if result is None:
        continue
    validate_common(
        suite,
        target,
        "cargo_test",
        result,
        admitted_support_checks,
        {"remote_execution_required": True},
    )

for suite, target in required_criterion_suites.items():
    _, result = load_current_result(suite)
    if result is None:
        continue
    expected_output = f"criterion/pi-perf-runs/{expected_instance}/{suite}"
    validate_common(
        suite,
        target,
        "criterion",
        result,
        admitted_producers,
        {"output_relative": expected_output},
    )

report = {
    "schema": "pi.perf.post_generation_producer_admission.v1",
    "generated_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    "source_commit": expected_commit,
    "source_dirty": False,
    "correlation_id": expected_correlation,
    "run_instance_id": expected_instance,
    "cargo_profile": expected_profile,
    "proof_scope": "producer_execution_receipts",
    "artifact_binding": "post_generation_evidence_inventory",
    "status": "ready" if not failures else "blocked",
    "failure_count": len(failures),
    "failures": failures,
    "producers": sorted(admitted_producers, key=lambda producer: producer["suite"]),
    "support_checks": sorted(
        admitted_support_checks, key=lambda check: check["suite"]
    ),
}
admission_path.write_text(
    json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
)
raise SystemExit(0 if not failures else 1)
PY
  then
    die "Exclusive post-generation producer admission failed"
  fi
  log_ok "Exclusive post-generation producer admission passed"
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
  local dst="$OUTPUT_DIR/results/$dst_name"
  if [[ -L "$src" ]]; then
    die "Refusing symlinked JSONL source: $src"
  fi
  if [[ -f "$src" ]]; then
    if [[ "$src" != "$dst" && ( -e "$dst" || -L "$dst" ) ]]; then
      die "Refusing preexisting collected JSONL destination: $dst"
    fi
    cp "$src" "$dst"
    artifact_count=$((artifact_count + 1))
    log_ok "Collected: $dst_name ($(wc -l < "$src") records)"
  fi
}

# Standard JSONL output paths
collect_jsonl "$OUTPUT_DIR/results/perf_bench_harness/extension_bench.jsonl" "extension_bench.jsonl"
if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" != true ]]; then
  collect_jsonl "$TARGET_DIR/perf/ext_bench_harness.jsonl" "ext_bench_harness.jsonl"
  collect_jsonl "$TARGET_DIR/perf/scenario_runner.jsonl" "scenario_runner.jsonl"
  collect_jsonl "$TARGET_DIR/perf/pijs_workload.jsonl" "pijs_workload.jsonl"
  collect_jsonl "$TARGET_DIR/perf/legacy_extension_workloads.jsonl" "legacy_extension_workloads.jsonl"
else
  for accepted_jsonl in \
    ext_bench_harness.jsonl \
    scenario_runner.jsonl \
    pijs_workload.jsonl \
    legacy_extension_workloads.jsonl; do
    if [[ -s "$OUTPUT_DIR/results/$accepted_jsonl" \
      && ! -L "$OUTPUT_DIR/results/$accepted_jsonl" ]]; then
      artifact_count=$((artifact_count + 1))
      log_ok "Collected: $accepted_jsonl ($(wc -l < "$OUTPUT_DIR/results/$accepted_jsonl") records)"
    fi
  done
fi
collect_jsonl "$TARGET_DIR/perf/$CARGO_PROFILE/pgo_pipeline_events.jsonl" "pgo_pipeline_events.jsonl"

if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" != true \
  && -f "$TARGET_DIR/perf/ext_bench_harness_report.json" ]]; then
  cp "$TARGET_DIR/perf/ext_bench_harness_report.json" "$OUTPUT_DIR/results/ext_bench_harness_report.json"
  artifact_count=$((artifact_count + 1))
  log_ok "Collected: ext_bench_harness_report.json"
elif [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true \
  && -s "$OUTPUT_DIR/results/ext_bench_harness_report.json" \
  && ! -L "$OUTPUT_DIR/results/ext_bench_harness_report.json" ]]; then
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

# Checked-in reports are historical/supporting inputs, not current-run evidence.
# Never admit them into the exclusive package where their unscoped rows could
# override current RCH measurements in a derived ratio.
if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" != true \
  && -d "$PROJECT_ROOT/tests/perf/reports" ]]; then
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
import re
import stat
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
import math
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
for record in workload_records:
    if record.get("iterations") != 2000 or record.get("tool_calls_per_iteration") != 10:
        continue
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


comparison_boundaries = {
    "cold_load_init": "matched_extension_cold_load",
    "per_call_dispatch_micro": "matched_extension_tool_dispatch",
    "full_e2e_long_session": "matched_full_session_workflow",
}
comparison_workload_shapes = {
    "cold_load_init": {
        "extension": "hello",
        "operation": "cold_load_init",
        "statistic": "p95",
    },
    "per_call_dispatch_micro": {
        "extension": "hello",
        "operation": "tool_call",
        "statistic": "mean",
    },
    "full_e2e_long_session": {
        "session_turns": 2000,
        "extension_loads_per_iteration": 2,
        "tool_calls_per_iteration": 10,
        "event_hooks_per_iteration": 1,
        "statistic": "elapsed",
    },
}


def validated_comparison_contract(record: dict, claim_scope: str):
    contract = record.get("comparison_contract")
    required_fields = {
        "schema",
        "claim_scope",
        "measurement_boundary",
        "release_claim_eligible",
        "host_fingerprint_sha256",
        "workload_shape",
    }
    if not isinstance(contract, dict) or set(contract) != required_fields:
        return None
    if (
        contract.get("schema") != "pi.perf.cross_runtime_comparison.v1"
        or contract.get("claim_scope") != claim_scope
        or contract.get("measurement_boundary") != comparison_boundaries[claim_scope]
        or contract.get("release_claim_eligible") is not True
    ):
        return None
    host_fingerprint = contract.get("host_fingerprint_sha256")
    if (
        not isinstance(host_fingerprint, str)
        or len(host_fingerprint) != 64
        or any(character not in "0123456789abcdef" for character in host_fingerprint)
    ):
        return None
    workload_shape = contract.get("workload_shape")
    if workload_shape != comparison_workload_shapes[claim_scope]:
        return None
    return contract


def positive_number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    parsed = float(value)
    return parsed if math.isfinite(parsed) and parsed > 0.0 else None


def matched_cross_runtime_ratios(
    rust_records: list[dict],
    rust_value,
    legacy_value,
    claim_scope: str,
):
    rust_by_contract: dict[str, list[float]] = {}
    for record in rust_records:
        contract = validated_comparison_contract(record, claim_scope)
        value = rust_value(record)
        if contract is None or value is None:
            continue
        contract_key = json.dumps(contract, sort_keys=True, separators=(",", ":"))
        rust_by_contract.setdefault(contract_key, []).append(value)

    legacy_by_runtime: dict[str, dict[str, list[float]]] = {"node": {}, "bun": {}}
    for record in legacy_records:
        if (
            record.get("legacy_pi_mono_executed") is not True
            or record.get("runtime_family") != "legacy_pi_mono_extension_loader"
            or record.get("runtime") != "legacy_pi_mono"
        ):
            continue
        contract = validated_comparison_contract(record, claim_scope)
        value = legacy_value(record)
        runtime_kind = legacy_runtime_kind(record)
        if contract is None or value is None or runtime_kind not in legacy_by_runtime:
            continue
        contract_key = json.dumps(contract, sort_keys=True, separators=(",", ":"))
        legacy_by_runtime[runtime_kind].setdefault(contract_key, []).append(value)

    common_contracts = (
        set(rust_by_contract)
        & set(legacy_by_runtime["node"])
        & set(legacy_by_runtime["bun"])
    )
    if len(common_contracts) != 1:
        return None, None, None
    contract_key = next(iter(common_contracts))
    rust_metric = mean(rust_by_contract[contract_key])
    node_metric = mean(legacy_by_runtime["node"][contract_key])
    bun_metric = mean(legacy_by_runtime["bun"][contract_key])
    if rust_metric is None or node_metric is None or bun_metric is None:
        return None, None, None
    return rust_metric, rust_metric / node_metric, rust_metric / bun_metric


matched_cold_abs_ms, cold_node_ratio, cold_bun_ratio = matched_cross_runtime_ratios(
    scenario_runner_records,
    lambda record: (
        positive_number(record.get("stats", {}).get("p95_ms"))
        if record.get("scenario") == "cold_start"
        and record.get("extension") == "hello"
        and isinstance(record.get("stats"), dict)
        else None
    ),
    lambda record: (
        positive_number(record.get("summary", {}).get("p95_ms"))
        if record.get("scenario") == "ext_load_init/load_init_cold"
        and record.get("extension") == "hello"
        and isinstance(record.get("summary"), dict)
        else None
    ),
    "cold_load_init",
)
matched_per_call_abs_us, per_call_node_ratio, per_call_bun_ratio = (
    matched_cross_runtime_ratios(
        scenario_runner_records,
        lambda record: (
            positive_number(record.get("per_call_us"))
            if record.get("scenario") == "tool_call"
            and record.get("extension") == "hello"
            else None
        ),
        lambda record: (
            positive_number(record.get("per_call_us"))
            if record.get("scenario") == "ext_tool_call/hello"
            and record.get("extension") == "hello"
            else None
        ),
        "per_call_dispatch_micro",
    )
)
matched_full_e2e_abs_ms, full_e2e_node_ratio, full_e2e_bun_ratio = (
    matched_cross_runtime_ratios(
        workload_records,
        lambda record: (
            positive_number(record.get("elapsed_ms"))
            if record.get("comparison_scenario") == "full_e2e_long_session"
            and record.get("session_turns") == 2000
            and record.get("extension_loads_per_iteration") == 2
            and record.get("tool_calls_per_iteration") == 10
            and record.get("event_hooks_per_iteration") == 1
            and record.get("tool_executions") == 20000
            and record.get("event_executions") == 2000
            else None
        ),
        lambda record: (
            positive_number(record.get("elapsed_ms"))
            if record.get("scenario") == "full_e2e_long_session"
            and record.get("extension") == "hello+pirate"
            else None
        ),
        "full_e2e_long_session",
    )
)
if matched_cold_abs_ms is not None:
    cold_abs_ms = matched_cold_abs_ms
if matched_per_call_abs_us is not None:
    per_call_abs_us = matched_per_call_abs_us
if matched_full_e2e_abs_ms is not None:
    full_e2e_abs_ms = matched_full_e2e_abs_ms

matched_comparison_basis = "matched_legacy_pi_mono_extension_loader"
full_e2e_node_ratio_basis = (
    matched_comparison_basis if full_e2e_node_ratio is not None else "missing"
)


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
    all_required_suites_passed = all(
        status == "pass" for status in suite_statuses.values()
    )

    if absolute_present and relative_present and all_required_suites_passed:
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
        ratio_basis(cold_node_ratio, matched_comparison_basis),
        cold_bun_ratio,
        ratio_basis(cold_bun_ratio, matched_comparison_basis),
        [
            ext_bench_path,
            ext_bench_report_path,
            scenario_runner_path,
            extension_bench_path,
            legacy_path,
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
        ratio_basis(per_call_node_ratio, matched_comparison_basis),
        per_call_bun_ratio,
        ratio_basis(per_call_bun_ratio, matched_comparison_basis),
        [scenario_runner_path, extension_bench_path, workload_path, legacy_path],
        "Per-call improvements are diagnostic and cannot substitute for full-session outcomes.",
    ),
    build_layer(
        "full_e2e_long_session",
        "Full end-to-end long-session workload",
        ["full-e2e", "long-session", "user-facing", "release-facing"],
        ["criterion_pijs", "bench_scenario"],
        "long_session_elapsed",
        full_e2e_abs_ms,
        "ms",
        full_e2e_node_ratio,
        full_e2e_node_ratio_basis,
        full_e2e_bun_ratio,
        ratio_basis(full_e2e_bun_ratio, matched_comparison_basis),
        [workload_path, legacy_path],
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
        and layer["evidence_state"] == "measured"
    )
    for layer in layers
}
portable_legacy_record_count = sum(
    1
    for record in legacy_records
    if record.get("runtime_family") == "portable_extension_api"
    and record.get("legacy_pi_mono_executed") is not True
)
true_legacy_record_count = sum(
    1
    for record in legacy_records
    if record.get("runtime_family") == "legacy_pi_mono_extension_loader"
    and record.get("legacy_pi_mono_executed") is True
)

invalidity_reasons = []
for dataset in source_datasets:
    if dataset["required"] and dataset["accepted_record_count"] == 0:
        invalidity_reasons.append(f"missing_current_run_source:{dataset['path']}")
    if dataset["rejected_record_count"] > 0:
        invalidity_reasons.append(f"mixed_source_lineage:{dataset['path']}")
if portable_legacy_record_count > 0:
    invalidity_reasons.append("portable_extension_api_not_release_comparator")
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
        "cross_runtime_comparison": {
            "contract_schema": "pi.perf.cross_runtime_comparison.v1",
            "legacy_pi_mono_executed_required": True,
            "exact_workload_and_host_contract_required": True,
            "portable_shim_record_count": portable_legacy_record_count,
            "true_legacy_pi_mono_record_count": true_legacy_record_count,
            "matched_layer_contracts": {
                "cold_load_init": cold_node_ratio is not None and cold_bun_ratio is not None,
                "per_call_dispatch_micro": (
                    per_call_node_ratio is not None and per_call_bun_ratio is not None
                ),
                "full_e2e_long_session": (
                    full_e2e_node_ratio is not None and full_e2e_bun_ratio is not None
                ),
            },
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
import math
import os
import re
import stat
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
        "required": True,
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


required_stage_evidence = {
    "evidence_class": "measured",
    "confidence": "high",
    "eligible_for_regression_gate": True,
    "measurement_method": "wall_clock_observation",
    "measurement_boundary": "production_session_stage_instrumentation",
    "measurement_contract_version": "production_session_stage_instrumentation.v1",
}
stage_evidence_rejections = []
stage_records = {}


def parse_non_negative_finite_metric(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    parsed = float(value)
    if not math.isfinite(parsed) or parsed < 0.0:
        return None
    return parsed


def parse_positive_finite_metric(value):
    parsed = parse_non_negative_finite_metric(value)
    return parsed if parsed is not None and parsed > 0.0 else None


for source_name, records in (
    ("scenario_runner", scenario_runner_records),
    ("pijs_workload", workload_records),
):
    for index, record in enumerate(records):
        if not isinstance(record, dict):
            continue
        if record.get("scenario") != "session_workload_matrix":
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

        evidence_mismatches = {
            field: {"expected": expected, "observed": record.get(field)}
            for field, expected in required_stage_evidence.items()
            if record.get(field) != expected
        }
        if evidence_mismatches:
            stage_evidence_rejections.append(
                {
                    "source_name": source_name,
                    "source_record_index": index,
                    "partition": partition,
                    "session_messages": session_messages,
                    "mismatches": evidence_mismatches,
                }
            )
            continue

        stage_attribution = record.get("stage_attribution")
        if not isinstance(stage_attribution, dict):
            stage_attribution = {}

        open_ms = parse_non_negative_finite_metric(record.get("open_ms"))
        if open_ms is None:
            open_ms = parse_non_negative_finite_metric(stage_attribution.get("open_ms"))
        append_ms = parse_non_negative_finite_metric(record.get("append_ms"))
        if append_ms is None:
            append_ms = parse_non_negative_finite_metric(stage_attribution.get("append_ms"))
        save_ms = parse_non_negative_finite_metric(record.get("save_ms"))
        if save_ms is None:
            save_ms = parse_non_negative_finite_metric(stage_attribution.get("save_ms"))
        index_ms = parse_non_negative_finite_metric(record.get("index_ms"))
        if index_ms is None:
            index_ms = parse_non_negative_finite_metric(record.get("session_index_ms"))
        if index_ms is None:
            index_ms = parse_non_negative_finite_metric(stage_attribution.get("index_ms"))
        wall_clock_ms = parse_positive_finite_metric(record.get("total_ms"))
        if wall_clock_ms is None:
            wall_clock_ms = parse_positive_finite_metric(record.get("elapsed_ms"))
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
            **{
                field: record.get(field)
                for field in required_stage_evidence
            },
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
            existing_swarm_score = int(isinstance(existing.get("swarm_metrics"), dict))
            candidate_swarm_score = int(isinstance(candidate.get("swarm_metrics"), dict))
            if (existing_score, existing_swarm_score) >= (
                candidate_score,
                candidate_swarm_score,
            ):
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
        value = parse_non_negative_finite_metric(source.get(key))
        if value is None:
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
        total_stage_ms = sum(
            value for value in stage_attribution.values() if value is not None
        )
        if all(value is None for value in stage_attribution.values()):
            total_stage_ms = None
        complete_stage_breakdown = (
            not missing_stage_keys
            and total_stage_ms is not None
            and total_stage_ms > 0.0
        )
        if complete_stage_breakdown:
            cells_with_complete_stage_breakdown += 1

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
        elif not complete_stage_breakdown:
            missing_reasons.append("invalid_stage_total:non_positive")
        if cell_wall_clock is None:
            missing_reasons.append("missing_primary_wall_clock")
        if primary_rust_vs_node_ratio is None or primary_rust_vs_bun_ratio is None:
            missing_reasons.append("missing_primary_relative_ratios")

        if source and complete_stage_breakdown:
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
                    **{
                        field: source.get(field)
                        for field in required_stage_evidence
                    },
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
        isinstance(reason, str)
        and reason.startswith(("missing_stage_metrics:", "invalid_stage_total:"))
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

def stable_file_identity(metadata):
    return (
        metadata.st_mode,
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


MAX_PERSISTENCE_CONTROL_BYTES = 4 * 1024 * 1024


def open_anchored_directory(parent_fd, child_name):
    try:
        path_metadata = os.stat(child_name, dir_fd=parent_fd, follow_symlinks=False)
        if not stat.S_ISDIR(path_metadata.st_mode):
            return None
        descriptor = os.open(
            child_name,
            os.O_RDONLY
            | getattr(os, "O_DIRECTORY", 0)
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_CLOEXEC", 0),
            dir_fd=parent_fd,
        )
    except OSError:
        return None
    try:
        descriptor_metadata = os.fstat(descriptor)
    except OSError:
        os.close(descriptor)
        return None
    if (
        not stat.S_ISDIR(descriptor_metadata.st_mode)
        or descriptor_metadata.st_dev != path_metadata.st_dev
        or descriptor_metadata.st_ino != path_metadata.st_ino
    ):
        os.close(descriptor)
        return None
    return descriptor, (path_metadata.st_dev, path_metadata.st_ino)


def read_stable_regular_at(parent_fd, file_name):
    path_metadata_before = os.stat(
        file_name, dir_fd=parent_fd, follow_symlinks=False
    )
    descriptor = os.open(
        file_name,
        os.O_RDONLY
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_CLOEXEC", 0),
        dir_fd=parent_fd,
    )
    try:
        descriptor_metadata_before = os.fstat(descriptor)
        if not stat.S_ISREG(descriptor_metadata_before.st_mode):
            raise ValueError(f"{file_name}: expected a regular file")
        if (
            descriptor_metadata_before.st_size < 0
            or descriptor_metadata_before.st_size > MAX_PERSISTENCE_CONTROL_BYTES
        ):
            raise ValueError(
                f"{file_name}: size {descriptor_metadata_before.st_size} exceeds "
                f"{MAX_PERSISTENCE_CONTROL_BYTES}-byte control limit"
            )
        chunks = []
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
        descriptor_metadata_after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    path_metadata_after = os.stat(
        file_name, dir_fd=parent_fd, follow_symlinks=False
    )
    contents = b"".join(chunks)
    expected_identity = stable_file_identity(path_metadata_before)
    if (
        not stat.S_ISREG(path_metadata_after.st_mode)
        or stable_file_identity(descriptor_metadata_before) != expected_identity
        or stable_file_identity(descriptor_metadata_after) != expected_identity
        or stable_file_identity(path_metadata_after) != expected_identity
        or len(contents) != path_metadata_before.st_size
    ):
        raise ValueError(f"{file_name}: file changed while read")
    return contents


def stable_regular_attestation_at(parent_fd, file_name):
    path_metadata_before = os.stat(
        file_name, dir_fd=parent_fd, follow_symlinks=False
    )
    descriptor = os.open(
        file_name,
        os.O_RDONLY
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_CLOEXEC", 0),
        dir_fd=parent_fd,
    )
    try:
        descriptor_metadata_before = os.fstat(descriptor)
        if not stat.S_ISREG(descriptor_metadata_before.st_mode):
            raise ValueError(f"{file_name}: expected a regular file")
        digest = hashlib.sha256()
        observed_size = 0
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
            observed_size += len(chunk)
        descriptor_metadata_after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    path_metadata_after = os.stat(
        file_name, dir_fd=parent_fd, follow_symlinks=False
    )
    expected_identity = stable_file_identity(path_metadata_before)
    if (
        not stat.S_ISREG(path_metadata_after.st_mode)
        or stable_file_identity(descriptor_metadata_before) != expected_identity
        or stable_file_identity(descriptor_metadata_after) != expected_identity
        or stable_file_identity(path_metadata_after) != expected_identity
        or observed_size != path_metadata_before.st_size
    ):
        raise ValueError(f"{file_name}: file changed while hashed")
    return observed_size, digest.hexdigest()


def manifest_artifact_contract_is_valid(
    child_fd, artifact_dir: Path, manifest: dict, overall_passed: bool
):
    case_files = (
        "result.json",
        "output.log",
        "test-log.jsonl",
        "artifact-index.jsonl",
        "{case_id}-fault-window-summary.json",
    )
    expected_result_files = [
        str(artifact_dir / "jsonl" / "result.json"),
        str(artifact_dir / "sqlite" / "result.json"),
        str(artifact_dir / "integrity-summary.json"),
    ]
    if manifest.get("result_files") != expected_result_files:
        return False
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, list) or len(artifacts) != 10:
        return False
    artifacts_by_path = {}
    for artifact in artifacts:
        if not isinstance(artifact, dict):
            return False
        artifact_path = artifact.get("path")
        if not isinstance(artifact_path, str) or artifact_path in artifacts_by_path:
            return False
        artifacts_by_path[artifact_path] = artifact

    expected_paths = {
        str(artifact_dir / case_id / file_name.format(case_id=case_id))
        for case_id in ("jsonl", "sqlite")
        for file_name in case_files
    }
    if set(artifacts_by_path) != expected_paths:
        return False

    for case_id in ("jsonl", "sqlite"):
        case_entries = [
            (
                file_name.format(case_id=case_id),
                artifacts_by_path[
                    str(
                        artifact_dir
                        / case_id
                        / file_name.format(case_id=case_id)
                    )
                ],
            )
            for file_name in case_files
        ]
        for _, entry in case_entries:
            present = entry.get("present")
            if type(present) is not bool:
                return False
            if not present:
                if overall_passed or "size_bytes" in entry or "sha256" in entry:
                    return False
                if "error" in entry and (
                    not isinstance(entry["error"], str) or not entry["error"].strip()
                ):
                    return False
        present_entries = [
            (file_name, entry)
            for file_name, entry in case_entries
            if entry["present"] is True
        ]
        if not present_entries:
            continue
        opened_case = open_anchored_directory(child_fd, case_id)
        if opened_case is None:
            return False
        case_fd, _ = opened_case
        try:
            for file_name, entry in present_entries:
                size_bytes = entry.get("size_bytes")
                sha256 = entry.get("sha256")
                if (
                    type(size_bytes) is not int
                    or size_bytes < 0
                    or not isinstance(sha256, str)
                    or re.fullmatch(r"[0-9a-f]{64}", sha256) is None
                ):
                    return False
                try:
                    observed_size, observed_sha256 = stable_regular_attestation_at(
                        case_fd, file_name
                    )
                except (OSError, ValueError):
                    return False
                if size_bytes != observed_size or sha256 != observed_sha256:
                    return False
        finally:
            os.close(case_fd)
    return True


fault_injection_root_resolved = None
fault_injection_root_fd = None
fault_injection_root_identity = None
fault_injection_candidates = []
try:
    fault_injection_root_resolved = fault_injection_root.resolve(strict=True)
    root_path_metadata = fault_injection_root_resolved.lstat()
    if not stat.S_ISDIR(root_path_metadata.st_mode):
        raise ValueError("persistence fault-injection root is not a directory")
    fault_injection_root_fd = os.open(
        fault_injection_root_resolved,
        os.O_RDONLY
        | getattr(os, "O_DIRECTORY", 0)
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_CLOEXEC", 0),
    )
    root_descriptor_metadata = os.fstat(fault_injection_root_fd)
    fault_injection_root_identity = stable_file_identity(root_path_metadata)
    if (
        not stat.S_ISDIR(root_descriptor_metadata.st_mode)
        or stable_file_identity(root_descriptor_metadata)
        != fault_injection_root_identity
    ):
        raise ValueError("persistence fault-injection root identity mismatch")
    for child_name in sorted(os.listdir(fault_injection_root_fd)):
        if not child_name or child_name in {".", ".."}:
            continue
        opened_child = open_anchored_directory(fault_injection_root_fd, child_name)
        if opened_child is None:
            continue
        child_fd, child_identity = opened_child
        try:
            fault_injection_candidates.append(
                (
                    child_name,
                    child_identity,
                    fault_injection_root_resolved / child_name / "run-manifest.json",
                )
            )
        finally:
            os.close(child_fd)
except (OSError, ValueError):
    fault_injection_root_resolved = None
    if fault_injection_root_fd is not None:
        os.close(fault_injection_root_fd)
        fault_injection_root_fd = None
fault_injection_manifest_path = None
fault_injection_summary_path = None
fault_injection_manifest_sha256 = None
fault_injection_manifest_size_bytes = None
fault_injection_summary_sha256 = None
fault_injection_summary_size_bytes = None
fault_injection_status = "missing"
fault_injection_summary = {}
fault_injection_manifest = {}
matching_fault_injection_runs = []


def opened_fault_injection_candidates(root_fd, candidates):
    for child_name, expected_child_identity, candidate in candidates:
        opened_child = open_anchored_directory(root_fd, child_name)
        if opened_child is None:
            continue
        child_fd, observed_child_identity = opened_child
        if observed_child_identity != expected_child_identity:
            os.close(child_fd)
            continue
        try:
            manifest_bytes = read_stable_regular_at(child_fd, "run-manifest.json")
        except (OSError, ValueError):
            os.close(child_fd)
            continue
        try:
            yield (
                child_name,
                child_fd,
                expected_child_identity,
                candidate,
                manifest_bytes,
            )
        finally:
            os.close(child_fd)


opened_candidates = (
    opened_fault_injection_candidates(
        fault_injection_root_fd, fault_injection_candidates
    )
    if fault_injection_root_fd is not None
    else ()
)
for (
    child_name,
    child_fd,
    child_identity,
    candidate,
    candidate_manifest_bytes,
) in opened_candidates:
    try:
        candidate_manifest = json.loads(candidate_manifest_bytes)
    except (UnicodeDecodeError, json.JSONDecodeError):
        continue
    if not isinstance(candidate_manifest, dict):
        continue
    if (
        candidate_manifest.get("schema")
        != "pi.e2e.persistence_fault_injection.manifest.v1"
        or candidate_manifest.get("terminal_state") != "complete"
    ):
        continue
    if candidate_manifest.get("correlation_id") != correlation_id:
        continue
    if candidate_manifest.get("run_id") != correlation_id:
        continue
    if candidate_manifest.get("source_commit") != source_commit:
        continue
    if candidate_manifest.get("source_dirty") is not source_dirty:
        continue
    source_tree_digest = candidate_manifest.get("source_tree_sha256")
    if not isinstance(source_tree_digest, str) or len(source_tree_digest) != 64:
        continue
    if any(character not in "0123456789abcdef" for character in source_tree_digest):
        continue
    if candidate_manifest.get("source_commit_final") != source_commit:
        continue
    if candidate_manifest.get("source_dirty_final") is not source_dirty:
        continue
    if candidate_manifest.get("source_tree_sha256_final") != source_tree_digest:
        continue
    if candidate_manifest.get("artifact_dir") != str(candidate.parent):
        continue
    if (
        candidate_manifest.get("runner_mode") != "rch"
        or candidate_manifest.get("rch_force_remote") is not True
        or candidate_manifest.get("rch_require_remote") is not True
        or candidate_manifest.get("execution_attestation") != "configuration_only"
    ):
        continue
    overall_passed = candidate_manifest.get("overall_passed")
    if type(overall_passed) is not bool:
        continue
    if not manifest_artifact_contract_is_valid(
        child_fd, candidate.parent, candidate_manifest, overall_passed
    ):
        continue
    exit_codes = candidate_manifest.get("exit_codes")
    if not isinstance(exit_codes, dict):
        continue
    if any(
        type(exit_codes.get(name)) is not int
        for name in ("jsonl", "sqlite", "summary_validation", "overall")
    ):
        continue
    required_exit_codes = {"jsonl", "sqlite", "summary_validation", "overall"}
    if set(exit_codes) != required_exit_codes:
        continue
    if any(exit_codes[name] < 0 or exit_codes[name] > 255 for name in ("jsonl", "sqlite")):
        continue
    if any(exit_codes[name] not in (0, 1) for name in ("summary_validation", "overall")):
        continue
    attempt_id = candidate_manifest.get("attempt_id")
    if not isinstance(attempt_id, str) or not attempt_id.strip():
        continue
    expected_overall_exit = (
        0
        if all(
            exit_codes[name] == 0
            for name in ("jsonl", "sqlite", "summary_validation")
        )
        else 1
    )
    if (
        exit_codes["overall"] != expected_overall_exit
        or overall_passed != (exit_codes["overall"] == 0)
    ):
        continue
    candidate_timestamp = parse_record_timestamp(candidate_manifest)
    if candidate_timestamp is None:
        continue
    if candidate_timestamp < run_started_at - source_clock_skew:
        continue
    if candidate_timestamp > datetime.now(timezone.utc) + source_clock_skew:
        continue

    summary_attestation = candidate_manifest.get("integrity_summary")
    if not isinstance(summary_attestation, dict):
        continue
    expected_summary_path = candidate.parent / "integrity-summary.json"
    if summary_attestation.get("path") != str(expected_summary_path):
        continue
    try:
        summary_bytes = read_stable_regular_at(child_fd, "integrity-summary.json")
    except (OSError, ValueError):
        continue
    summary_size_bytes = summary_attestation.get("size_bytes")
    if (
        type(summary_size_bytes) is not int
        or summary_size_bytes <= 0
        or summary_size_bytes != len(summary_bytes)
    ):
        continue
    summary_sha256 = summary_attestation.get("sha256")
    if (
        not isinstance(summary_sha256, str)
        or re.fullmatch(r"[0-9a-f]{64}", summary_sha256) is None
        or summary_sha256 != hashlib.sha256(summary_bytes).hexdigest()
    ):
        continue
    try:
        candidate_summary = json.loads(summary_bytes)
    except (UnicodeDecodeError, json.JSONDecodeError):
        continue
    if not isinstance(candidate_summary, dict):
        continue
    if (
        candidate_summary.get("schema")
        != "pi.e2e.persistence_fault_injection.summary.v1"
        or candidate_summary.get("terminal_state") != "summary_validated"
    ):
        continue
    if candidate_summary.get("assertions") != {
        "process_failure_windows": {
            "pre_flush": "in_process_drop",
            "mid_flush": "hard_exit",
            "post_flush": "hard_exit",
        },
        "observed_invariants": [
            "persisted_baseline_preserved",
            "no_duplicate_messages",
            "observed_message_order_exact",
        ],
        "power_loss_durability_attested": False,
    }:
        continue
    if any(
        candidate_summary.get(field) != candidate_manifest.get(field)
        for field in (
            "run_id",
            "attempt_id",
            "correlation_id",
            "source_commit",
            "source_dirty",
            "source_tree_sha256",
            "source_commit_final",
            "source_dirty_final",
            "source_tree_sha256_final",
            "runner_mode",
            "rch_force_remote",
            "rch_require_remote",
            "execution_attestation",
        )
    ):
        continue
    if (
        candidate_summary.get("source_dirty") is not source_dirty
        or candidate_summary.get("source_dirty_final") is not source_dirty
        or candidate_summary.get("runner_mode") != "rch"
        or candidate_summary.get("rch_force_remote") is not True
        or candidate_summary.get("rch_require_remote") is not True
        or candidate_summary.get("execution_attestation") != "configuration_only"
    ):
        continue
    if candidate_summary.get("source_tree_stable") is not True:
        continue
    validation_passed = candidate_summary.get("validation_passed")
    if type(validation_passed) is not bool or validation_passed != overall_passed:
        continue
    summary_timestamp = parse_record_timestamp(candidate_summary)
    summary_started_at = parse_record_timestamp(
        {"timestamp": candidate_summary.get("run_started_at")}
    )
    if (
        summary_timestamp is None
        or summary_started_at is None
        or summary_started_at < run_started_at - source_clock_skew
        or summary_started_at > summary_timestamp
        or summary_timestamp > candidate_timestamp
        or summary_timestamp > datetime.now(timezone.utc) + source_clock_skew
    ):
        continue
    cases = candidate_summary.get("cases")
    if not isinstance(cases, list) or len(cases) != 2:
        continue
    cases_by_id = {
        case.get("case_id"): case
        for case in cases
        if isinstance(case, dict)
        and case.get("case_id") in {"jsonl", "sqlite"}
        and type(case.get("passed")) is bool
    }
    if set(cases_by_id) != {"jsonl", "sqlite"}:
        continue
    required_case_checks = {
        "test_command_passed",
        "output_log_regular",
        "result_schema_valid",
        "result_identity_current",
        "fault_log_emitted",
        "summary_artifact_indexed",
        "summary_artifact_schema_valid",
        "summary_artifact_bytes_verified",
        "summary_artifact_path_confined",
        "diagnostic_log_schema_valid",
        "artifact_index_schema_valid",
        "diagnostic_sequence_valid",
        "diagnostic_trace_bound",
        "correlation_id_current",
        "test_identity_current",
    }
    case_contracts_valid = True
    for case_id, case in cases_by_id.items():
        checks = case.get("checks")
        test_log_records = case.get("test_log_records")
        artifact_records = case.get("artifact_records")
        if (
            case.get("result_file") != str(candidate.parent / case_id / "result.json")
            or not isinstance(checks, dict)
            or set(checks) != required_case_checks
            or any(type(value) is not bool for value in checks.values())
            or case["passed"] != all(checks.values())
            or type(test_log_records) is not int
            or test_log_records < 0
            or type(artifact_records) is not int
            or artifact_records < 0
            or (case["passed"] and (test_log_records == 0 or artifact_records == 0))
        ):
            case_contracts_valid = False
            break
    if not case_contracts_valid:
        continue
    if validation_passed != (
        all(case["passed"] for case in cases_by_id.values())
        and candidate_summary.get("source_tree_stable") is True
    ):
        continue
    if exit_codes["summary_validation"] != (0 if validation_passed else 1):
        continue
    try:
        final_child_metadata = os.stat(
            child_name, dir_fd=fault_injection_root_fd, follow_symlinks=False
        )
    except OSError:
        continue
    if (
        not stat.S_ISDIR(final_child_metadata.st_mode)
        or (final_child_metadata.st_dev, final_child_metadata.st_ino) != child_identity
    ):
        continue
    matching_fault_injection_runs.append(
        (
            candidate_timestamp,
            str(candidate),
            candidate,
            candidate_manifest,
            hashlib.sha256(candidate_manifest_bytes).hexdigest(),
            len(candidate_manifest_bytes),
            expected_summary_path,
            candidate_summary,
            hashlib.sha256(summary_bytes).hexdigest(),
            len(summary_bytes),
        )
    )

if fault_injection_root_fd is not None:
    try:
        final_root_path_metadata = fault_injection_root_resolved.lstat()
        final_root_descriptor_metadata = os.fstat(fault_injection_root_fd)
        if (
            stable_file_identity(final_root_path_metadata)
            != fault_injection_root_identity
            or stable_file_identity(final_root_descriptor_metadata)
            != fault_injection_root_identity
        ):
            matching_fault_injection_runs = []
    except OSError:
        matching_fault_injection_runs = []
    os.close(fault_injection_root_fd)

if matching_fault_injection_runs:
    (
        _,
        _,
        fault_injection_manifest_path,
        fault_injection_manifest,
        fault_injection_manifest_sha256,
        fault_injection_manifest_size_bytes,
        fault_injection_summary_path,
        fault_injection_summary,
        fault_injection_summary_sha256,
        fault_injection_summary_size_bytes,
    ) = max(matching_fault_injection_runs, key=lambda candidate: (candidate[0], candidate[1]))
    fault_injection_status = (
        "pass" if fault_injection_manifest.get("overall_passed") is True else "fail"
    )

def sha256_file(path: Path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def evaluate_idle_rss_control():
    control_path = target_dir / "perf" / "release_evidence" / "idle_memory_rss.json"
    evidence = {
        "path": str(control_path),
        "threshold_bytes": 50 * 1024 * 1024,
        "status": "missing",
        "failure_reasons": [],
    }
    if control_path.is_symlink() or not control_path.is_file():
        evidence["failure_reasons"].append("missing_regular_control")
        return "missing", evidence
    try:
        control = load_json(control_path)
    except (OSError, json.JSONDecodeError) as error:
        evidence["failure_reasons"].append(f"invalid_control_json:{error}")
        return "missing", evidence
    for field, expected in (
        ("schema", "pi.perf.idle_rss_measurement.v1"),
        ("run_id", correlation_id),
        ("correlation_id", correlation_id),
        ("source_commit", source_commit),
        ("source_dirty", False),
        ("process_name", "pi"),
        ("idle_state", "startup_before_user_input"),
        ("cargo_profile", "release"),
        ("build_command", "cargo build --bin pi --release"),
    ):
        if control.get(field) != expected:
            evidence["failure_reasons"].append(f"{field}_mismatch")
    samples = control.get("samples")
    sample_count = control.get("sample_count")
    rss_bytes = control.get("rss_bytes")
    if (
        not isinstance(samples, list)
        or type(sample_count) is not int
        or sample_count < 5
        or len(samples) != sample_count
    ):
        evidence["failure_reasons"].append("invalid_samples")
    else:
        sample_rss = [
            sample.get("rss_bytes")
            for sample in samples
            if isinstance(sample, dict)
            and type(sample.get("rss_bytes")) is int
            and sample.get("rss_bytes") > 0
            and sample.get("process_name") == "pi"
        ]
        sample_pids = [
            sample.get("pid")
            for sample in samples
            if isinstance(sample, dict) and type(sample.get("pid")) is int
        ]
        if (
            len(sample_rss) != sample_count
            or len(sample_pids) != sample_count
            or len(set(sample_pids)) != sample_count
            or any(pid <= 0 for pid in sample_pids)
            or rss_bytes != max(sample_rss)
            or control.get("rss_spread_bytes") != max(sample_rss) - min(sample_rss)
        ):
            evidence["failure_reasons"].append("sample_aggregate_mismatch")
    binary_path_raw = control.get("binary_path")
    binary_sha256 = control.get("binary_sha256")
    expected_binary_path = target_dir / "release" / "pi"
    if not isinstance(binary_path_raw, str) or not binary_path_raw:
        evidence["failure_reasons"].append("missing_binary_path")
    else:
        binary_path = Path(binary_path_raw)
        try:
            expected_binary = expected_binary_path.resolve(strict=True)
            observed_binary = binary_path.resolve(strict=True)
        except OSError:
            evidence["failure_reasons"].append("missing_release_binary")
        else:
            if binary_path.is_symlink() or not binary_path.is_file() or observed_binary != expected_binary:
                evidence["failure_reasons"].append("release_binary_path_mismatch")
            elif not isinstance(binary_sha256, str) or sha256_file(binary_path) != binary_sha256:
                evidence["failure_reasons"].append("release_binary_digest_mismatch")
    evidence["rss_bytes"] = rss_bytes
    evidence["control_sha256"] = sha256_file(control_path)
    if evidence["failure_reasons"]:
        return "missing", evidence
    if type(rss_bytes) is not int or rss_bytes <= 0:
        evidence["failure_reasons"].append("invalid_rss_bytes")
        return "missing", evidence
    evidence["status"] = "pass" if rss_bytes <= evidence["threshold_bytes"] else "fail"
    return evidence["status"], evidence


memory_status, memory_evidence = evaluate_idle_rss_control()

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
    and memory_status == "pass"
    and correctness_status == "pass"
    and security_status == "pass"
    and cells_with_complete_stage_breakdown == required_cell_count
    and len(missing_cells) == 0
    and cells_with_complete_swarm_metrics == required_cell_count
    and len(swarm_missing_cells) == 0
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
        "required_evidence_contract": required_stage_evidence,
        "evidence_rejections": stage_evidence_rejections,
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
            "idle_memory_rss": memory_evidence,
            "fault_injection_script": str(fault_injection_script),
            "fault_injection_manifest_path": (
                str(fault_injection_manifest_path)
                if fault_injection_manifest_path is not None
                else None
            ),
            "fault_injection_summary_path": (
                str(fault_injection_summary_path)
                if fault_injection_summary_path is not None
                else None
            ),
            "fault_injection_manifest": (
                {
                    "path": str(fault_injection_manifest_path),
                    "sha256": fault_injection_manifest_sha256,
                    "size_bytes": fault_injection_manifest_size_bytes,
                }
                if fault_injection_manifest_path is not None
                else None
            ),
            "fault_injection_summary": (
                {
                    "path": str(fault_injection_summary_path),
                    "sha256": fault_injection_summary_sha256,
                    "size_bytes": fault_injection_summary_size_bytes,
                }
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
            "non_measured_matrix_evidence",
            "memory_regression",
            "memory_regression_unverified",
            "correctness_regression",
            "correctness_regression_unverified",
            "security_regression",
            "security_regression_unverified",
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

if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
  if ! verify_current_clean_source_identity "Exclusive post-generation evidence admission"; then
    die "Exclusive post-generation evidence requires a stable clean source identity"
  fi
fi

POST_GENERATION_CONTRACT_PATH="$OUTPUT_DIR/results/post_generation_evidence_contract.json"
post_generation_exit=0
if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
  if PROJECT_ROOT="$PROJECT_ROOT" \
  OUTPUT_DIR="$OUTPUT_DIR" \
  CORRELATION_ID="$CORRELATION_ID" \
  GIT_COMMIT_FULL="$GIT_COMMIT_FULL" \
  GIT_DIRTY="$GIT_DIRTY" \
  POST_GENERATION_CONTRACT_PATH="$POST_GENERATION_CONTRACT_PATH" \
  python3 - <<'PY'
import hashlib
import json
import math
import os
import re
from datetime import datetime, timezone
from pathlib import Path

output_dir = Path(os.environ["OUTPUT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
expected_correlation_id = os.environ["CORRELATION_ID"]
expected_source_commit = os.environ["GIT_COMMIT_FULL"]
expected_source_dirty = os.environ["GIT_DIRTY"] == "true"
report_path = Path(os.environ["POST_GENERATION_CONTRACT_PATH"])
phase1_path = output_dir / "results" / "phase1_matrix_validation.json"
stratification_path = output_dir / "results" / "extension_benchmark_stratification.json"
perf_sli_path = project_root / "docs" / "perf_sli_matrix.json"
failures = []


def load_artifact(path: Path, expected_schema: str):
    if not path.is_file():
        failures.append({"path": str(path), "reason": "missing_artifact"})
        return {}
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        failures.append(
            {"path": str(path), "reason": "invalid_json", "detail": str(error)}
        )
        return {}
    if not isinstance(payload, dict):
        failures.append({"path": str(path), "reason": "non_object_artifact"})
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


def validate_source_datasets(path: Path, payload, expected_basenames):
    datasets = payload.get("source_datasets", [])
    if not isinstance(datasets, list) or not datasets:
        failures.append({"path": str(path), "reason": "missing_source_datasets"})
        return
    observed_basenames = [
        Path(dataset.get("path")).name
        for dataset in datasets
        if isinstance(dataset, dict)
        and isinstance(dataset.get("path"), str)
        and dataset.get("path")
    ]
    if len(observed_basenames) != len(datasets) or set(observed_basenames) != set(
        expected_basenames
    ) or len(observed_basenames) != len(expected_basenames):
        failures.append(
            {
                "path": str(path),
                "reason": "source_dataset_identity_mismatch",
                "expected_basenames": sorted(expected_basenames),
                "observed_basenames": sorted(observed_basenames),
            }
        )
    for dataset in datasets:
        if not isinstance(dataset, dict):
            failures.append({"path": str(path), "reason": "invalid_source_dataset"})
            continue
        accepted = dataset.get("accepted_record_count")
        rejected = dataset.get("rejected_record_count")
        required = dataset.get("required")
        source_path = dataset.get("path")
        expected_sha256 = dataset.get("sha256")
        accepted_valid = type(accepted) is int and accepted >= 0
        rejected_valid = type(rejected) is int and rejected >= 0
        required_valid = type(required) is bool
        if not accepted_valid or not rejected_valid or not required_valid:
            failures.append(
                {
                    "path": str(path),
                    "reason": "invalid_source_dataset_metadata",
                    "source_path": source_path,
                }
            )
        verify_source_bytes = (
            required is True
            or (accepted_valid and accepted > 0)
            or (rejected_valid and rejected > 0)
            or expected_sha256 is not None
        )
        if required is True and (not accepted_valid or accepted <= 0):
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
            try:
                observed_sha256 = hashlib.sha256(Path(source_path).read_bytes()).hexdigest()
            except OSError as error:
                failures.append(
                    {
                        "path": str(path),
                        "reason": "source_dataset_read_error",
                        "source_path": source_path,
                        "detail": str(error),
                    }
                )
                continue
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
        if not rejected_valid or rejected != 0:
            failures.append(
                {
                    "path": str(path),
                    "reason": "mixed_source_lineage",
                    "source_path": dataset.get("path"),
                    "rejected_record_count": rejected,
                }
            )


phase1 = load_artifact(phase1_path, "pi.perf.phase1_matrix_validation.v1")
validate_source_datasets(
    phase1_path,
    phase1,
    {"scenario_runner.jsonl", "pijs_workload.jsonl"},
)
consumption_contract = phase1.get("consumption_contract")
if not isinstance(consumption_contract, dict):
    consumption_contract = {}
if consumption_contract.get("artifact_ready_for_phase5") is not True:
    failures.append({"path": str(phase1_path), "reason": "phase1_not_ready"})
regression_guards = phase1.get("regression_guards", {})
for guard_name in ("memory", "correctness", "security"):
    guard_status = (
        regression_guards.get(guard_name)
        if isinstance(regression_guards, dict)
        else None
    )
    if guard_status != "pass":
        failures.append(
            {
                "path": str(phase1_path),
                "reason": f"{guard_name}_regression_unverified",
                "observed": guard_status,
            }
        )
stage_summary = phase1.get("stage_summary")
if not isinstance(stage_summary, dict):
    stage_summary = {}
required_phase1_stage_evidence = {
    "evidence_class": "measured",
    "confidence": "high",
    "eligible_for_regression_gate": True,
    "measurement_method": "wall_clock_observation",
    "measurement_boundary": "production_session_stage_instrumentation",
    "measurement_contract_version": "production_session_stage_instrumentation.v1",
}
if stage_summary.get("required_evidence_contract") != required_phase1_stage_evidence:
    failures.append(
        {
            "path": str(phase1_path),
            "reason": "invalid_required_stage_evidence_contract",
        }
    )
matrix_cells = phase1.get("matrix_cells", [])
matrix_requirements = phase1.get("matrix_requirements", {})
if not isinstance(matrix_requirements, dict):
    matrix_requirements = {}
required_cell_count = matrix_requirements.get("required_cell_count")
required_partitions = matrix_requirements.get("required_partition_tags")
required_sizes = matrix_requirements.get("required_session_message_sizes")

try:
    perf_sli = json.loads(perf_sli_path.read_text(encoding="utf-8"))
except (OSError, UnicodeError, json.JSONDecodeError) as error:
    failures.append(
        {
            "path": str(perf_sli_path),
            "reason": "invalid_perf_sli_contract",
            "detail": str(error),
        }
    )
    perf_sli = {}
if not isinstance(perf_sli, dict):
    failures.append({"path": str(perf_sli_path), "reason": "non_object_perf_sli_contract"})
    perf_sli = {}

reporting_contract = perf_sli.get("reporting_contract")
if not isinstance(reporting_contract, dict):
    reporting_contract = {}
benchmark_partitions = perf_sli.get("benchmark_partitions")
if not isinstance(benchmark_partitions, dict):
    benchmark_partitions = {}
expected_partitions = reporting_contract.get("required_partition_tags")
realistic_ids = benchmark_partitions.get("realistic_long_session")
matched_state_ids = benchmark_partitions.get("matched_state")
declared_partition_tags = benchmark_partitions.get("partition_tags")


def parse_contract_session_sizes(values, prefix):
    if not isinstance(values, list):
        return None
    parsed_sizes = []
    pattern = re.compile(rf"{re.escape(prefix)}_(\d+)([km]?)")
    for value in values:
        if not isinstance(value, str):
            return None
        match = pattern.fullmatch(value.strip().lower())
        if match is None:
            return None
        multiplier = {"": 1, "k": 1_000, "m": 1_000_000}[match.group(2)]
        parsed_sizes.append(int(match.group(1)) * multiplier)
    return parsed_sizes


def finite_number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    try:
        parsed = float(value)
    except (OverflowError, ValueError):
        return None
    return parsed if math.isfinite(parsed) else None


def positive_finite_number(value):
    parsed = finite_number(value)
    return parsed is not None and parsed > 0.0


def canonical_cell_identity(partition, size):
    return (
        isinstance(partition, str)
        and bool(partition)
        and type(size) is int
        and size > 0
    )


realistic_sizes = parse_contract_session_sizes(realistic_ids, "realistic")
matched_state_sizes = parse_contract_session_sizes(matched_state_ids, "matched_state")
expected_sizes = (
    realistic_sizes
    if realistic_sizes is not None and realistic_sizes == matched_state_sizes
    else None
)
canonical_dimensions_valid = (
    isinstance(expected_partitions, list)
    and bool(expected_partitions)
    and expected_partitions == declared_partition_tags
    and all(isinstance(value, str) and value.strip() for value in expected_partitions)
    and len(set(expected_partitions)) == len(expected_partitions)
    and isinstance(expected_sizes, list)
    and bool(expected_sizes)
    and all(type(value) is int and value > 0 for value in expected_sizes)
    and len(set(expected_sizes)) == len(expected_sizes)
)
if not canonical_dimensions_valid:
    failures.append(
        {"path": str(perf_sli_path), "reason": "invalid_canonical_matrix_dimensions"}
    )
    expected_partitions = []
    expected_sizes = []
declared_dimensions_valid = (
    isinstance(required_partitions, list)
    and bool(required_partitions)
    and all(isinstance(partition, str) and partition.strip() for partition in required_partitions)
    and len(set(required_partitions)) == len(required_partitions)
    and isinstance(required_sizes, list)
    and bool(required_sizes)
    and all(type(size) is int and size > 0 for size in required_sizes)
    and len(set(required_sizes)) == len(required_sizes)
)
expected_cell_count = (
    len(required_partitions) * len(required_sizes)
    if declared_dimensions_valid
    else None
)
canonical_cell_count = len(expected_partitions) * len(expected_sizes)
if required_partitions != expected_partitions or required_sizes != expected_sizes:
    failures.append(
        {
            "path": str(phase1_path),
            "reason": "matrix_dimensions_differ_from_perf_sli_contract",
            "declared_partitions": required_partitions,
            "expected_partitions": expected_partitions,
            "declared_sizes": required_sizes,
            "expected_sizes": expected_sizes,
        }
    )
if type(required_cell_count) is not int or required_cell_count <= 0:
    failures.append({"path": str(phase1_path), "reason": "invalid_required_cell_count"})
elif required_cell_count != expected_cell_count or required_cell_count != canonical_cell_count:
    failures.append(
        {
            "path": str(phase1_path),
            "reason": "incomplete_matrix_cartesian_product",
            "expected": canonical_cell_count,
            "observed": required_cell_count,
        }
    )
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
    required_swarm_groups = {
        "latency_quantiles_ms": ("p50", "p95", "p99", "p999"),
        "queue_depth": ("p50", "p95", "p99", "p999", "max"),
        "resource_usage": ("rss_mb", "cpu_pct"),
        "component_breakdown_ms": ("tool", "provider", "extension", "session"),
        "stage_breakdown_ms": ("open", "append", "save", "index"),
        "host_capacity": ("target_cpu_cores", "observed_cpu_cores", "mem_total_mb"),
    }
    expected_matrix_keys = {
        (partition, size)
        for partition in expected_partitions
        for size in expected_sizes
    }
    observed_matrix_keys = set()
    for index, cell in enumerate(matrix_cells):
        if not isinstance(cell, dict):
            failures.append(
                {
                    "path": str(phase1_path),
                    "reason": "non_object_matrix_cell",
                    "cell_index": index,
                }
            )
            continue
        cell_partition = cell.get("workload_partition")
        cell_size = cell.get("session_messages")
        cell_identity_valid = canonical_cell_identity(cell_partition, cell_size)
        if not cell_identity_valid:
            failures.append(
                {
                    "path": str(phase1_path),
                    "reason": "invalid_matrix_cell_identity",
                    "cell_index": index,
                    "workload_partition": cell_partition,
                    "session_messages": cell_size,
                }
            )
        else:
            cell_key = (cell_partition, cell_size)
            if cell_key in observed_matrix_keys:
                failures.append(
                    {
                        "path": str(phase1_path),
                        "reason": "duplicate_matrix_cell",
                        "cell_index": index,
                        "workload_partition": cell_partition,
                        "session_messages": cell_size,
                    }
                )
            observed_matrix_keys.add(cell_key)
        attribution = cell.get("stage_attribution")
        if not isinstance(attribution, dict):
            attribution = {}
        lineage = cell.get("lineage")
        if not isinstance(lineage, dict):
            lineage = {}
        swarm_metrics = cell.get("swarm_metrics")
        invalid_swarm_metrics = []
        if not isinstance(swarm_metrics, dict) or set(swarm_metrics) != set(
            required_swarm_groups
        ):
            invalid_swarm_metrics.append("group_set")
            swarm_metrics = {}
        for group_name, required_keys in required_swarm_groups.items():
            group = swarm_metrics.get(group_name)
            if not isinstance(group, dict) or set(group) != set(required_keys):
                invalid_swarm_metrics.append(group_name)
                continue
            for metric_name in required_keys:
                metric_value = finite_number(group.get(metric_name))
                if metric_value is None or metric_value < 0.0:
                    invalid_swarm_metrics.append(f"{group_name}.{metric_name}")
        primary_e2e = cell.get("primary_e2e")
        invalid_primary_metrics = []
        if not isinstance(primary_e2e, dict) or set(primary_e2e) != {
            "wall_clock_ms",
            "rust_vs_node_ratio",
            "rust_vs_bun_ratio",
        }:
            invalid_primary_metrics.append("field_set")
            primary_e2e = {}
        for metric_name in (
            "wall_clock_ms",
            "rust_vs_node_ratio",
            "rust_vs_bun_ratio",
        ):
            metric_value = finite_number(primary_e2e.get(metric_name))
            if metric_value is None or metric_value <= 0.0:
                invalid_primary_metrics.append(metric_name)
        parsed_stages = {
            key: finite_number(attribution.get(key)) for key in required_stages
        }
        invalid_stages = [
            key
            for key, value in parsed_stages.items()
            if value is None or value < 0.0
        ]
        reported_stage_total = attribution.get("total_stage_ms")
        parsed_stage_total = finite_number(reported_stage_total)
        observed_stage_total = (
            sum(parsed_stages[key] for key in required_stages)
            if not invalid_stages
            else None
        )
        stage_total_valid = (
            parsed_stage_total is not None
            and parsed_stage_total > 0.0
            and observed_stage_total is not None
            and math.isclose(
                parsed_stage_total,
                observed_stage_total,
                rel_tol=1e-9,
                abs_tol=1e-9,
            )
        )
        invalid_evidence = [
            field
            for field, expected in required_phase1_stage_evidence.items()
            if not isinstance(lineage, dict) or lineage.get(field) != expected
        ]
        if (
            not cell_identity_valid
            or cell.get("status") != "pass"
            or cell.get("missing_reasons") != []
            or invalid_stages
            or not stage_total_valid
            or invalid_evidence
            or invalid_swarm_metrics
            or invalid_primary_metrics
        ):
            failures.append(
                {
                    "path": str(phase1_path),
                    "reason": "invalid_matrix_cell",
                    "cell_index": index,
                    "invalid_stages": invalid_stages,
                    "reported_stage_total_ms": reported_stage_total,
                    "observed_stage_total_ms": observed_stage_total,
                    "stage_total_valid": stage_total_valid,
                    "invalid_evidence": invalid_evidence,
                    "invalid_swarm_metrics": invalid_swarm_metrics,
                    "invalid_primary_metrics": invalid_primary_metrics,
                }
            )
    if observed_matrix_keys != expected_matrix_keys:
        failures.append(
            {
                "path": str(phase1_path),
                "reason": "matrix_cell_identity_set_mismatch",
                "missing": [
                    {"workload_partition": partition, "session_messages": size}
                    for partition, size in sorted(expected_matrix_keys - observed_matrix_keys)
                ],
                "unexpected": [
                    {"workload_partition": partition, "session_messages": size}
                    for partition, size in sorted(observed_matrix_keys - expected_matrix_keys)
                ],
            }
        )
    stage_summary_valid = (
        stage_summary.get("cells_with_complete_stage_breakdown")
        == canonical_cell_count
        and stage_summary.get("cells_missing_stage_breakdown") == 0
        and stage_summary.get("covered_cells") == canonical_cell_count
        and stage_summary.get("missing_cells") == []
    )
    if not stage_summary_valid:
        failures.append(
            {"path": str(phase1_path), "reason": "stage_summary_mismatch"}
        )
    swarm_summary = phase1.get("swarm_summary")
    if not isinstance(swarm_summary, dict):
        swarm_summary = {}
    swarm_summary_valid = (
        swarm_summary.get("required_latency_quantiles")
        == list(required_swarm_groups["latency_quantiles_ms"])
        and swarm_summary.get("required_queue_depth_quantiles")
        == list(required_swarm_groups["queue_depth"])
        and swarm_summary.get("required_resource_usage_keys")
        == list(required_swarm_groups["resource_usage"])
        and swarm_summary.get("required_component_breakdown_keys")
        == list(required_swarm_groups["component_breakdown_ms"])
        and swarm_summary.get("required_stage_breakdown_keys")
        == list(required_swarm_groups["stage_breakdown_ms"])
        and swarm_summary.get("cells_with_complete_swarm_metrics")
        == canonical_cell_count
        and swarm_summary.get("cells_missing_swarm_metrics") == 0
        and swarm_summary.get("missing_cells") == []
    )
    if not swarm_summary_valid:
        failures.append(
            {"path": str(phase1_path), "reason": "swarm_summary_mismatch"}
        )
    primary_outcomes = phase1.get("primary_outcomes")
    if not isinstance(primary_outcomes, dict):
        primary_outcomes = {}
    primary_outcomes_valid = (
        primary_outcomes.get("status") == "pass"
        and primary_outcomes.get("missing_reasons") == []
        and all(
            positive_finite_number(primary_outcomes.get(metric_name))
            for metric_name in (
                "wall_clock_ms",
                "rust_vs_node_ratio",
                "rust_vs_bun_ratio",
            )
        )
    )
    if not primary_outcomes_valid:
        failures.append(
            {"path": str(phase1_path), "reason": "primary_outcomes_mismatch"}
        )

stratification = load_artifact(
    stratification_path, "pi.perf.extension_benchmark_stratification.v1"
)
validate_source_datasets(
    stratification_path,
    stratification,
    {
        "scenario_runner.jsonl",
        "pijs_workload.jsonl",
        "extension_bench.jsonl",
        "ext_bench_harness.jsonl",
        "legacy_extension_workloads.jsonl",
    },
)
required_layer_ids = (
    "cold_load_init",
    "per_call_dispatch_micro",
    "full_e2e_long_session",
)
required_layers = set(required_layer_ids)
layers = stratification.get("layers", [])
seen_layers = set()
measured_layers = set()
observed_layer_coverage = {layer_id: False for layer_id in required_layer_ids}
observed_matched_contracts = {layer_id: False for layer_id in required_layer_ids}
matched_comparison_basis = "matched_legacy_pi_mono_extension_loader"


if not isinstance(layers, list) or len(layers) != len(required_layer_ids):
    failures.append(
        {
            "path": str(stratification_path),
            "reason": "invalid_layer_count",
            "expected": len(required_layer_ids),
            "observed": len(layers) if isinstance(layers, list) else None,
        }
    )
    layers = layers if isinstance(layers, list) else []

for index, layer in enumerate(layers):
    if not isinstance(layer, dict):
        failures.append(
            {
                "path": str(stratification_path),
                "reason": "invalid_layer_record",
                "layer_index": index,
            }
        )
        continue
    layer_id = layer.get("layer_id")
    if (
        not isinstance(layer_id, str)
        or layer_id not in required_layers
        or layer_id in seen_layers
    ):
        failures.append(
            {
                "path": str(stratification_path),
                "reason": "unexpected_or_duplicate_layer",
                "layer_index": index,
                "layer_id": layer_id,
            }
        )
        continue
    seen_layers.add(layer_id)
    absolute = layer.get("absolute_metrics", {})
    relative = layer.get("relative_metrics", {})
    absolute_valid = isinstance(absolute, dict) and positive_finite_number(
        absolute.get("value")
    )
    node_ratio_valid = isinstance(relative, dict) and positive_finite_number(
        relative.get("rust_vs_node_ratio")
    )
    bun_ratio_valid = isinstance(relative, dict) and positive_finite_number(
        relative.get("rust_vs_bun_ratio")
    )
    ratio_pair_valid = node_ratio_valid and bun_ratio_valid
    ratio_bases_valid = (
        isinstance(relative, dict)
        and relative.get("rust_vs_node_ratio_basis") == matched_comparison_basis
        and relative.get("rust_vs_bun_ratio_basis") == matched_comparison_basis
    )
    matched_contract = ratio_pair_valid and ratio_bases_valid
    observed_matched_contracts[layer_id] = matched_contract
    evidence_measured = (
        layer.get("evidence_state") == "measured"
        and layer.get("confidence") == "high"
    )
    observed_layer_coverage[layer_id] = (
        absolute_valid and matched_contract and evidence_measured
    )
    if evidence_measured:
        measured_layers.add(layer_id)
    if not absolute_valid or not matched_contract:
        failures.append(
            {
                "path": str(stratification_path),
                "reason": "invalid_layer_comparison_contract",
                "layer_id": layer_id,
                "absolute_metric_valid": absolute_valid,
                "node_ratio_valid": node_ratio_valid,
                "bun_ratio_valid": bun_ratio_valid,
                "ratio_bases_valid": ratio_bases_valid,
            }
        )

if measured_layers != required_layers:
    failures.append(
        {
            "path": str(stratification_path),
            "reason": "required_layers_not_measured",
            "expected": sorted(required_layers),
            "observed": sorted(layer for layer in measured_layers if isinstance(layer, str)),
        }
    )

claim_integrity = stratification.get("claim_integrity", {})
if not isinstance(claim_integrity, dict):
    claim_integrity = {}
cross_runtime = claim_integrity.get("cross_runtime_comparison", {})
if not isinstance(cross_runtime, dict):
    cross_runtime = {}
portable_record_count = cross_runtime.get("portable_shim_record_count")
true_legacy_record_count = cross_runtime.get("true_legacy_pi_mono_record_count")
matched_layer_contracts = cross_runtime.get("matched_layer_contracts")
cross_runtime_contract_valid = (
    cross_runtime.get("contract_schema") == "pi.perf.cross_runtime_comparison.v1"
    and cross_runtime.get("legacy_pi_mono_executed_required") is True
    and cross_runtime.get("exact_workload_and_host_contract_required") is True
    and type(portable_record_count) is int
    and portable_record_count == 0
    and type(true_legacy_record_count) is int
    and true_legacy_record_count == 10
    and isinstance(matched_layer_contracts, dict)
    and set(matched_layer_contracts) == required_layers
    and all(type(matched_layer_contracts.get(layer_id)) is bool for layer_id in required_layer_ids)
    and matched_layer_contracts == observed_matched_contracts
)
if not cross_runtime_contract_valid:
    failures.append(
        {
            "path": str(stratification_path),
            "reason": "invalid_cross_runtime_comparison_contract",
            "portable_shim_record_count": portable_record_count,
            "true_legacy_pi_mono_record_count": true_legacy_record_count,
            "declared_matched_layer_contracts": matched_layer_contracts,
            "observed_matched_layer_contracts": observed_matched_contracts,
        }
    )

claim_guard = claim_integrity.get("cherry_pick_guard", {})
if not isinstance(claim_guard, dict):
    claim_guard = {}
declared_layer_coverage = claim_guard.get("layer_coverage")
if (
    claim_guard.get("requires_all_layers_for_global_claim") is not True
    or not isinstance(declared_layer_coverage, dict)
    or set(declared_layer_coverage) != required_layers
    or any(type(declared_layer_coverage.get(layer_id)) is not bool for layer_id in required_layer_ids)
    or declared_layer_coverage != observed_layer_coverage
):
    failures.append(
        {
            "path": str(stratification_path),
            "reason": "invalid_layer_coverage_claim",
            "declared": declared_layer_coverage,
            "observed": observed_layer_coverage,
        }
    )

invalidity_reasons = claim_guard.get("invalidity_reasons")
invalidity_reasons_valid = isinstance(invalidity_reasons, list) and all(
    isinstance(reason, str) for reason in invalidity_reasons
)
required_partition_tags = claim_integrity.get("required_partition_tags")
required_partition_tags_valid = (
    isinstance(required_partition_tags, list)
    and bool(required_partition_tags)
    and required_partition_tags == expected_partitions
    and all(isinstance(tag, str) and tag.strip() for tag in required_partition_tags)
    and len(set(required_partition_tags)) == len(required_partition_tags)
)
partition_coverage = claim_integrity.get("partition_coverage")
partition_coverage_valid = (
    required_partition_tags_valid
    and isinstance(partition_coverage, dict)
    and set(partition_coverage) == set(required_partition_tags)
    and all(type(partition_coverage.get(tag)) is bool for tag in required_partition_tags)
)
all_partitions_covered = partition_coverage_valid and all(
    partition_coverage[tag] is True for tag in required_partition_tags
)
expected_global_claim_valid = (
    measured_layers == required_layers
    and all(observed_layer_coverage.values())
    and all(observed_matched_contracts.values())
    and cross_runtime_contract_valid
    and all_partitions_covered
    and invalidity_reasons_valid
    and not invalidity_reasons
)
if not invalidity_reasons_valid or not partition_coverage_valid:
    failures.append(
        {
            "path": str(stratification_path),
            "reason": "invalid_global_claim_inputs",
            "invalidity_reasons": invalidity_reasons,
            "required_partition_tags": required_partition_tags,
            "partition_coverage": partition_coverage,
        }
    )
if claim_guard.get("global_claim_valid") is not expected_global_claim_valid:
    failures.append(
        {
            "path": str(stratification_path),
            "reason": "global_claim_validity_mismatch",
            "declared": claim_guard.get("global_claim_valid"),
            "expected": expected_global_claim_valid,
        }
    )
if expected_global_claim_valid is not True:
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
    # Name the failures in the log so a gate transcript explains itself
    # without the JSON artifact (the DSR lane only keeps stdout).
    if [[ -f "$POST_GENERATION_CONTRACT_PATH" ]]; then
      python3 - "$POST_GENERATION_CONTRACT_PATH" "$STRATIFICATION_PATH" <<'PY' | while IFS= read -r line; do log_warn "  $line"; done
import json
import sys
from pathlib import Path

with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
failures = [f for f in payload.get("failures", []) if isinstance(f, dict)]
for failure in failures[:40]:
    reason = failure.get("reason", "unknown")
    detail = {k: v for k, v in failure.items() if k not in {"reason", "path"}}
    print(f"contract failure: {reason} {json.dumps(detail, sort_keys=True)[:200]}")
if len(failures) > 40:
    print(f"contract failure: ... {len(failures) - 40} more")
# The phase-1 cells inherit their node/bun ratios from the stratification's
# full_e2e_long_session layer, so show every layer's evidence state.
stratification_path = Path(sys.argv[2])
if stratification_path.is_file():
    with stratification_path.open(encoding="utf-8") as handle:
        stratification = json.load(handle)
    for layer in stratification.get("layers", []):
        if not isinstance(layer, dict):
            continue
        relative = layer.get("relative_metrics") or {}
        print(
            f"layer {layer.get('layer_id')}: evidence={layer.get('evidence_state')} "
            f"confidence={layer.get('confidence')} "
            f"node_ratio={relative.get('rust_vs_node_ratio')} "
            f"({relative.get('rust_vs_node_ratio_basis')}) "
            f"bun_ratio={relative.get('rust_vs_bun_ratio')} "
            f"({relative.get('rust_vs_bun_ratio_basis')})"
        )
PY
    fi
  fi
  artifact_count=$((artifact_count + 1))
else
  log_warn "Exclusive post-generation evidence skipped: $post_generation_skip_reason"
fi

POST_GENERATION_BUDGET_DIR="$OUTPUT_DIR/results/perf_budgets_post_generation"
post_generation_budget_exit=0
post_generation_package_status="pending"
post_generation_budget_status="skip"
post_generation_inventory_sha256=""
post_generation_package_sha256=""
post_generation_package_file_count=0
post_generation_package_size_bytes=0
POST_GENERATION_STAGE_RELATIVE=""
POST_GENERATION_EVIDENCE_DIR=""
declare -a POST_GENERATION_RUNNER_ARGS=("${CARGO_RUNNER_ARGS[@]}")
if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
  if ! verify_current_clean_source_identity "RCH post-generation staging precondition"; then
    die "RCH post-generation staging source identity is not stable"
  fi

post_generation_stage_key="$RUN_INSTANCE_ID"
POST_GENERATION_STAGE_RELATIVE=".rch-tmp/pi-perf-evidence/$post_generation_stage_key"
if ! PROJECT_ROOT="$PROJECT_ROOT" \
  OUTPUT_DIR="$OUTPUT_DIR" \
  TARGET_DIR="$TARGET_DIR" \
  POST_GENERATION_STAGE_RELATIVE="$POST_GENERATION_STAGE_RELATIVE" \
  GIT_COMMIT_FULL="$GIT_COMMIT_FULL" \
  CORRELATION_ID="$CORRELATION_ID" \
  RUN_INSTANCE_ID="$RUN_INSTANCE_ID" \
  CARGO_RUNNER_MODE="$CARGO_RUNNER_MODE" \
  CARGO_PROFILE="$CARGO_PROFILE" \
  SELECTED_SUITES="${SELECTED_SUITES[*]}" \
  python3 - <<'PY'
import hashlib
import json
import os
import stat
from pathlib import Path, PurePosixPath

project_root = Path(os.environ["PROJECT_ROOT"]).resolve(strict=True)
output_dir = Path(os.environ["OUTPUT_DIR"]).resolve(strict=True)
target_dir = Path(os.environ["TARGET_DIR"]).resolve(strict=True)
stage_relative = PurePosixPath(os.environ["POST_GENERATION_STAGE_RELATIVE"])
if stage_relative.is_absolute() or not stage_relative.parts or any(
    part in {"", ".", ".."} for part in stage_relative.parts
):
    raise SystemExit("invalid post-generation evidence stage path")
stage = project_root.joinpath(*stage_relative.parts)

no_follow = getattr(os, "O_NOFOLLOW", 0)
directory_flags = os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | no_follow
project_fd = os.open(project_root, directory_flags)
cursor_fd = os.dup(project_fd)
try:
    for index, part in enumerate(stage_relative.parts):
        final = index == len(stage_relative.parts) - 1
        created = False
        try:
            metadata = os.stat(part, dir_fd=cursor_fd, follow_symlinks=False)
        except FileNotFoundError:
            os.mkdir(part, 0o700, dir_fd=cursor_fd)
            metadata = os.stat(part, dir_fd=cursor_fd, follow_symlinks=False)
            created = True
        if final and not created:
            raise SystemExit(f"refusing preexisting post-generation evidence stage: {stage}")
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise SystemExit(f"post-generation evidence stage has unsafe component: {part}")
        next_fd = os.open(part, directory_flags, dir_fd=cursor_fd)
        opened_metadata = os.fstat(next_fd)
        if (metadata.st_dev, metadata.st_ino) != (
            opened_metadata.st_dev,
            opened_metadata.st_ino,
        ):
            os.close(next_fd)
            raise SystemExit(f"post-generation evidence stage component changed: {part}")
        os.close(cursor_fd)
        cursor_fd = next_fd
    stage_fd = cursor_fd
    cursor_fd = None
finally:
    if cursor_fd is not None:
        os.close(cursor_fd)

output_fd = os.open(output_dir, directory_flags)
target_fd = os.open(target_dir, directory_flags)
source_anchors = sorted(
    ((output_dir, output_fd), (target_dir, target_fd)),
    key=lambda item: len(item[0].parts),
    reverse=True,
)

selected_suites = set(os.environ["SELECTED_SUITES"].split())
criterion_run_root = (
    target_dir / "criterion" / "pi-perf-runs" / os.environ["RUN_INSTANCE_ID"]
)
required_files = [
    (
        output_dir / "results" / "extension_benchmark_stratification.json",
        PurePosixPath("extension_benchmark_stratification.json"),
        "post-generation derivation",
    ),
    (
        output_dir / "results" / "phase1_matrix_validation.json",
        PurePosixPath("phase1_matrix_validation.json"),
        "post-generation derivation",
    ),
    (
        output_dir / "results" / "post_generation_producer_admission.json",
        PurePosixPath("post_generation_producer_admission.json"),
        "post-generation producer admission",
    ),
    (
        output_dir / "results" / "pijs_workload.jsonl",
        PurePosixPath("pijs_workload.jsonl"),
        "pijs_workload",
    ),
    (
        target_dir / "release" / "pi",
        PurePosixPath("release/pi"),
        "release binary build",
    ),
    (
        target_dir / "perf" / "release_evidence" / "binary_size_measurement.json",
        PurePosixPath("release_evidence/binary_size_measurement.json"),
        "release binary measurement",
    ),
    (
        target_dir / "perf" / "release_evidence" / "cold_load_measurement.json",
        PurePosixPath("release_evidence/cold_load_measurement.json"),
        "Criterion extension measurement",
    ),
    (
        target_dir / "perf" / "release_evidence" / "idle_memory_rss.json",
        PurePosixPath("release_evidence/idle_memory_rss.json"),
        "idle RSS measurement",
    ),
]
criterion_required_inputs = {
    "criterion_extensions": [
        "ext_load_init/load_init_cold/hello/new/estimates.json",
        "ext_load_init/load_init_cold/pirate/new/estimates.json",
        "ext_policy/evaluate/prompt_allow/new/estimates.json",
        "ext_policy/evaluate/prompt_prompt/new/estimates.json",
        "ext_policy/evaluate/prompt_deny/new/estimates.json",
        "ext_policy/evaluate/strict_allow/new/estimates.json",
        "ext_policy/evaluate/strict_deny/new/estimates.json",
        "ext_policy/evaluate/permissive_allow/new/estimates.json",
        "ext_protocol/parse_and_validate/host_call_small/new/estimates.json",
        "ext_protocol/parse_and_validate/log_big/new/estimates.json",
    ],
    "criterion_pijs": [],
    "criterion_system": [
        "startup/version/warm/new/estimates.json",
        "startup/help/warm/new/estimates.json",
        "startup/list_models/warm/new/estimates.json",
    ],
    "criterion_semantic_context": [
        "semantic_context/graph_build_cold/large_workspace/new/sample.json",
        "semantic_context/graph_build_warm/large_workspace/new/sample.json",
        "semantic_context/incremental_update/large_workspace/new/sample.json",
        "semantic_context/planning/large_workspace/new/sample.json",
        "semantic_context/bundle_serialization/large_workspace/new/sample.json",
    ],
}
criterion_expected_targets = {
    "criterion_extensions": "extensions",
    "criterion_pijs": "pijs_workload",
    "criterion_system": "system",
    "criterion_semantic_context": "semantic_context",
}
for suite, relative_paths in criterion_required_inputs.items():
    if suite not in selected_suites:
        continue
    suite_root = criterion_run_root / suite
    required_files.extend(
        (
            suite_root.joinpath(*PurePosixPath(relative).parts),
            PurePosixPath("criterion") / relative,
            suite,
        )
        for relative in relative_paths
    )
if "criterion_semantic_context" in selected_suites:
    required_files.append(
        (
            criterion_run_root
            / "criterion_semantic_context"
            / "context_intelligence"
            / "perf_budget.json",
            PurePosixPath("context_intelligence/perf_budget.json"),
            "criterion_semantic_context",
        )
    )

entries = []
destinations = set()


def stable_identity(metadata):
    return (
        stat.S_IFMT(metadata.st_mode),
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def source_anchor_and_relative(source: Path):
    for anchor, anchor_fd in source_anchors:
        try:
            relative = source.relative_to(anchor)
        except ValueError:
            continue
        if relative.parts and all(part not in {"", ".", ".."} for part in relative.parts):
            return anchor_fd, relative
    raise SystemExit(f"post-generation evidence source is outside admitted roots: {source}")


def open_source_parent(source: Path):
    anchor_fd, relative = source_anchor_and_relative(source)
    parent_fd = os.dup(anchor_fd)
    try:
        for part in relative.parts[:-1]:
            metadata = os.stat(part, dir_fd=parent_fd, follow_symlinks=False)
            if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
                raise SystemExit(f"post-generation evidence source has unsafe ancestor: {source}")
            next_fd = os.open(part, directory_flags, dir_fd=parent_fd)
            opened_metadata = os.fstat(next_fd)
            if (metadata.st_dev, metadata.st_ino) != (
                opened_metadata.st_dev,
                opened_metadata.st_ino,
            ):
                os.close(next_fd)
                raise SystemExit(f"post-generation evidence source ancestor changed: {source}")
            os.close(parent_fd)
            parent_fd = next_fd
        return parent_fd, relative.parts[-1]
    except BaseException:
        os.close(parent_fd)
        raise


def open_destination_parent(relative: PurePosixPath):
    parent_fd = os.dup(stage_fd)
    try:
        for part in relative.parts[:-1]:
            try:
                metadata = os.stat(part, dir_fd=parent_fd, follow_symlinks=False)
            except FileNotFoundError:
                os.mkdir(part, 0o700, dir_fd=parent_fd)
                metadata = os.stat(part, dir_fd=parent_fd, follow_symlinks=False)
            if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
                raise SystemExit(f"staged evidence destination has unsafe ancestor: {relative}")
            next_fd = os.open(part, directory_flags, dir_fd=parent_fd)
            opened_metadata = os.fstat(next_fd)
            if (metadata.st_dev, metadata.st_ino) != (
                opened_metadata.st_dev,
                opened_metadata.st_ino,
            ):
                os.close(next_fd)
                raise SystemExit(f"staged evidence destination ancestor changed: {relative}")
            os.close(parent_fd)
            parent_fd = next_fd
        return parent_fd, relative.parts[-1]
    except BaseException:
        os.close(parent_fd)
        raise


def copy_regular_from_parent(
    source_parent_fd, source_name, source_label: Path, relative: PurePosixPath, source_metadata=None
):
    relative_text = relative.as_posix()
    if relative.is_absolute() or any(part in {"", ".", ".."} for part in relative.parts):
        raise SystemExit(f"invalid staged evidence path: {relative_text}")
    if relative_text in destinations:
        raise SystemExit(f"duplicate staged evidence path: {relative_text}")
    destinations.add(relative_text)
    if source_metadata is None:
        source_metadata = os.stat(source_name, dir_fd=source_parent_fd, follow_symlinks=False)
    if stat.S_ISLNK(source_metadata.st_mode) or not stat.S_ISREG(source_metadata.st_mode):
        raise SystemExit(f"post-generation evidence source is not a regular file: {source_label}")

    source_flags = os.O_RDONLY | no_follow
    destination_flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | no_follow
    digest = hashlib.sha256()
    source_fd = os.open(source_name, source_flags, dir_fd=source_parent_fd)
    try:
        opened_metadata = os.fstat(source_fd)
        if stable_identity(source_metadata) != stable_identity(opened_metadata):
            raise SystemExit(f"post-generation evidence source changed before copy: {source_label}")
        destination_parent_fd, destination_name = open_destination_parent(relative)
        try:
            destination_fd = os.open(
                destination_name,
                destination_flags,
                0o600,
                dir_fd=destination_parent_fd,
            )
            try:
                copied = 0
                while True:
                    chunk = os.read(source_fd, 1024 * 1024)
                    if not chunk:
                        break
                    digest.update(chunk)
                    view = memoryview(chunk)
                    while view:
                        written = os.write(destination_fd, view)
                        if written <= 0:
                            raise SystemExit(
                                f"short write while staging evidence: {relative_text}"
                            )
                        copied += written
                        view = view[written:]
                destination_metadata = os.fstat(destination_fd)
                if copied != destination_metadata.st_size:
                    raise SystemExit(f"staged evidence size mismatch: {relative_text}")
            finally:
                os.close(destination_fd)
        finally:
            os.close(destination_parent_fd)
        final_opened_metadata = os.fstat(source_fd)
        final_path_metadata = os.stat(
            source_name, dir_fd=source_parent_fd, follow_symlinks=False
        )
        if (
            stable_identity(source_metadata) != stable_identity(final_opened_metadata)
            or stable_identity(source_metadata) != stable_identity(final_path_metadata)
            or copied != opened_metadata.st_size
        ):
            raise SystemExit(
                f"post-generation evidence source changed while copied: {source_label}"
            )
    finally:
        os.close(source_fd)
    entries.append(
        {
            "logical_input_id": f"file:{relative_text}",
            "path": relative_text,
            "sha256": digest.hexdigest(),
            "size_bytes": copied,
        }
    )


def copy_regular_file(source: Path, relative: PurePosixPath):
    source_parent_fd, source_name = open_source_parent(source)
    try:
        copy_regular_from_parent(source_parent_fd, source_name, source, relative)
    finally:
        os.close(source_parent_fd)


def sha256_regular_file(path: Path):
    digest = hashlib.sha256()
    parent_fd, name = open_source_parent(path)
    try:
        before = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        descriptor = os.open(name, os.O_RDONLY | no_follow, dir_fd=parent_fd)
        try:
            opened = os.fstat(descriptor)
            if not stat.S_ISREG(opened.st_mode) or stable_identity(before) != stable_identity(opened):
                raise SystemExit(f"evidence input is not a stable regular file: {path}")
            while True:
                chunk = os.read(descriptor, 1024 * 1024)
                if not chunk:
                    break
                digest.update(chunk)
            after = os.fstat(descriptor)
            after_path = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
            if stable_identity(before) != stable_identity(after) or stable_identity(before) != stable_identity(after_path):
                raise SystemExit(f"evidence input changed while hashed: {path}")
        finally:
            os.close(descriptor)
    finally:
        os.close(parent_fd)
    return digest.hexdigest()


def read_stable_regular_file(path: Path):
    parent_fd, name = open_source_parent(path)
    try:
        before = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
        descriptor = os.open(name, os.O_RDONLY | no_follow, dir_fd=parent_fd)
        try:
            opened = os.fstat(descriptor)
            if not stat.S_ISREG(opened.st_mode) or stable_identity(before) != stable_identity(opened):
                raise SystemExit(f"evidence input is not a stable regular file: {path}")
            chunks = []
            while True:
                chunk = os.read(descriptor, 1024 * 1024)
                if not chunk:
                    break
                chunks.append(chunk)
            after = os.fstat(descriptor)
            after_path = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
            if stable_identity(before) != stable_identity(after) or stable_identity(before) != stable_identity(after_path):
                raise SystemExit(f"evidence input changed while read: {path}")
            return b"".join(chunks)
        finally:
            os.close(descriptor)
    finally:
        os.close(parent_fd)


for suite in sorted(selected_suites.intersection(criterion_required_inputs)):
    suite_result_path = output_dir / "results" / suite / "result.json"
    try:
        suite_result = json.loads(read_stable_regular_file(suite_result_path))
    except (FileNotFoundError, json.JSONDecodeError) as error:
        raise SystemExit(
            f"Criterion producer {suite} has no parseable current suite result: {error}"
        ) from error
    if (
        suite_result.get("schema") != "pi.perf.suite_result.v1"
        or suite_result.get("suite_name") != suite
        or suite_result.get("target") != criterion_expected_targets[suite]
        or suite_result.get("kind") != "criterion"
        or suite_result.get("status") != "pass"
        or type(suite_result.get("exit_code")) is not int
        or suite_result.get("exit_code") != 0
        or suite_result.get("correlation_id") != os.environ["CORRELATION_ID"]
        or suite_result.get("run_instance_id") != os.environ["RUN_INSTANCE_ID"]
        or suite_result.get("source_commit") != os.environ["GIT_COMMIT_FULL"]
        or suite_result.get("source_dirty") is not False
        or suite_result.get("output_relative")
        != f"criterion/pi-perf-runs/{os.environ['RUN_INSTANCE_ID']}/{suite}"
        or suite_result.get("runner_mode") != os.environ["CARGO_RUNNER_MODE"]
        or suite_result.get("profile") != os.environ["CARGO_PROFILE"]
        or type(suite_result.get("remote_execution_verified")) is not bool
        or suite_result.get("remote_execution_verified")
        != (os.environ["CARGO_RUNNER_MODE"] == "rch")
    ):
        raise SystemExit(
            f"Criterion producer {suite} did not pass in the current correlation"
        )

pijs_artifact = output_dir / "results" / "pijs_workload.jsonl"
if pijs_artifact.is_file():
    admitted_pijs_records = []
    pijs_bytes = read_stable_regular_file(pijs_artifact)
    try:
        pijs_text = pijs_bytes.decode("utf-8")
    except UnicodeDecodeError as error:
        raise SystemExit(f"invalid UTF-8 PiJS evidence: {error}") from error
    for line_number, line in enumerate(pijs_text.splitlines(), start=1):
        if not line.strip():
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise SystemExit(
                f"invalid PiJS evidence at line {line_number}: {error}"
            ) from error
        if isinstance(record, dict) and record.get("eligible_for_regression_gate") is True:
            admitted_pijs_records.append(record)
    if admitted_pijs_records:
        claimed_binaries = {
            (record.get("binary_path"), record.get("binary_sha256"))
            for record in admitted_pijs_records
        }
        if len(claimed_binaries) != 1:
            raise SystemExit(
                "eligible PiJS records must share one binary_path and binary_sha256"
            )
        claimed_path, claimed_sha256 = claimed_binaries.pop()
        if (
            not isinstance(claimed_path, str)
            or not claimed_path
            or not isinstance(claimed_sha256, str)
            or len(claimed_sha256) != 64
        ):
            raise SystemExit("eligible PiJS records have invalid binary provenance")
        binary_candidates = [
            criterion_run_root / "criterion_pijs" / "pijs_workload"
        ]
        pijs_binary = None
        for candidate in binary_candidates:
            try:
                candidate_metadata = candidate.lstat()
            except FileNotFoundError:
                continue
            if stat.S_ISLNK(candidate_metadata.st_mode) or not stat.S_ISREG(
                candidate_metadata.st_mode
            ):
                continue
            observed_sha256 = sha256_regular_file(candidate)
            if observed_sha256 == claimed_sha256:
                pijs_binary = candidate
                break
        if pijs_binary is None:
            raise SystemExit(
                "eligible PiJS evidence has no locally available digest-matching executable"
            )
        copy_regular_file(
            pijs_binary,
            PurePosixPath("perf/examples/pijs_workload"),
        )

for source, destination, producer_suite in required_files:
    try:
        copy_regular_file(source, destination)
    except FileNotFoundError as error:
        raise SystemExit(
            f"missing required input {destination.as_posix()} from successful {producer_suite}"
        ) from error
if not entries:
    raise SystemExit("post-generation evidence stage contains no regular files")

entries.sort(key=lambda entry: entry["path"])
inventory = {
    "schema": "pi.perf.post_generation_evidence_inventory.v1",
    "source_commit": os.environ["GIT_COMMIT_FULL"],
    "source_dirty": False,
    "correlation_id": os.environ["CORRELATION_ID"],
    "run_instance_id": os.environ["RUN_INSTANCE_ID"],
    "entries": entries,
}
inventory_bytes = (json.dumps(inventory, indent=2, sort_keys=True) + "\n").encode("utf-8")
inventory_fd = os.open(
    "post_generation_evidence_inventory.json",
    os.O_WRONLY | os.O_CREAT | os.O_EXCL | no_follow,
    0o600,
    dir_fd=stage_fd,
)
try:
    view = memoryview(inventory_bytes)
    while view:
        written = os.write(inventory_fd, view)
        if written <= 0:
            raise SystemExit("short write while creating post-generation evidence inventory")
        view = view[written:]
finally:
    os.close(inventory_fd)
PY
then
  die "Failed to create the post-generation evidence package"
fi
POST_GENERATION_EVIDENCE_DIR="$POST_GENERATION_STAGE_RELATIVE"
log_ok "Post-generation evidence package retained for audit: $POST_GENERATION_STAGE_RELATIVE"

if [[ "$CARGO_RUNNER_MODE" == "rch" ]]; then
  for required_env in PERF_EVIDENCE_DIR PI_PERF_POST_GENERATION PI_PERF_EXPECTED_SOURCE_COMMIT CI_CORRELATION_ID PI_PERF_STRICT; do
    case ",${RCH_ENV_ALLOWLIST:-}," in
      *",$required_env,"*) ;;
      *) RCH_ENV_ALLOWLIST="${RCH_ENV_ALLOWLIST:+$RCH_ENV_ALLOWLIST,}$required_env" ;;
    esac
  done
  export RCH_ENV_ALLOWLIST

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
PI_PERF_STRICT=1 \
PI_PERF_EXPECTED_SOURCE_COMMIT="$GIT_COMMIT_FULL" \
CI_CORRELATION_ID="$CORRELATION_ID" \
RCH_REQUIRE_REMOTE=1 \
RCH_QUIET=0 \
RCH_VISIBILITY=summary \
"${POST_GENERATION_RUNNER_ARGS[@]}" test --test perf_budgets --profile "$CARGO_PROFILE" \
  ci_enforced_budgets_fail_on_regression_or_missing_data -- --exact --nocapture \
  > "$POST_GENERATION_BUDGET_DIR/stdout.log" \
  2> "$POST_GENERATION_BUDGET_DIR/stderr.log" \
  || post_generation_budget_exit=$?

post_generation_package_summary=""
if post_generation_package_summary="$(
  PROJECT_ROOT="$PROJECT_ROOT" \
    POST_GENERATION_STAGE_RELATIVE="$POST_GENERATION_STAGE_RELATIVE" \
    GIT_COMMIT_FULL="$GIT_COMMIT_FULL" \
    CORRELATION_ID="$CORRELATION_ID" \
    RUN_INSTANCE_ID="$RUN_INSTANCE_ID" \
    python3 - <<'PY'
import hashlib
import json
import os
import stat
from pathlib import Path, PurePosixPath

project_root = Path(os.environ["PROJECT_ROOT"]).resolve(strict=True)
stage_relative = PurePosixPath(os.environ["POST_GENERATION_STAGE_RELATIVE"])
if stage_relative.is_absolute() or not stage_relative.parts or any(
    part in {"", ".", ".."} for part in stage_relative.parts
):
    raise SystemExit("invalid retained post-generation evidence stage path")

stage = project_root
for part in stage_relative.parts:
    stage = stage / part
    metadata = stage.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise SystemExit(f"retained post-generation evidence has unsafe ancestor: {stage}")

inventory_path = stage / "post_generation_evidence_inventory.json"
inventory_metadata = inventory_path.lstat()
if stat.S_ISLNK(inventory_metadata.st_mode) or not stat.S_ISREG(
    inventory_metadata.st_mode
):
    raise SystemExit("retained post-generation evidence inventory is not a regular file")


def stable_regular_digest(path: Path, collect_bytes: bool = False):
    before = path.lstat()
    if stat.S_ISLNK(before.st_mode) or not stat.S_ISREG(before.st_mode):
        raise SystemExit(f"retained post-generation evidence is not a regular file: {path}")
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(path, flags)
    try:
        opened = os.fstat(descriptor)
        identity = lambda value: (
            stat.S_IFMT(value.st_mode),
            value.st_dev,
            value.st_ino,
            value.st_size,
            value.st_mtime_ns,
            value.st_ctime_ns,
        )
        if identity(before) != identity(opened):
            raise SystemExit(f"retained post-generation evidence changed before read: {path}")
        digest = hashlib.sha256()
        chunks = [] if collect_bytes else None
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
            if chunks is not None:
                chunks.append(chunk)
        after = os.fstat(descriptor)
        after_path = path.lstat()
        if identity(before) != identity(after) or identity(before) != identity(after_path):
            raise SystemExit(f"retained post-generation evidence changed while read: {path}")
        content = b"".join(chunks) if chunks is not None else None
        return digest.hexdigest(), before.st_size, content
    finally:
        os.close(descriptor)


inventory_sha256, _, inventory_bytes = stable_regular_digest(
    inventory_path, collect_bytes=True
)
try:
    inventory = json.loads(inventory_bytes)
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"retained post-generation evidence inventory is invalid: {error}") from error
expected_header = {
    "schema": "pi.perf.post_generation_evidence_inventory.v1",
    "source_commit": os.environ["GIT_COMMIT_FULL"],
    "source_dirty": False,
    "correlation_id": os.environ["CORRELATION_ID"],
    "run_instance_id": os.environ["RUN_INSTANCE_ID"],
}
for key, expected in expected_header.items():
    if inventory.get(key) != expected:
        raise SystemExit(
            f"retained post-generation evidence inventory {key} mismatch"
        )
entries = inventory.get("entries")
if not isinstance(entries, list) or not entries:
    raise SystemExit("retained post-generation evidence inventory has no entries")

expected = {}
for entry in entries:
    if not isinstance(entry, dict):
        raise SystemExit("retained post-generation evidence inventory entry is invalid")
    relative = entry.get("path")
    relative_path = PurePosixPath(relative) if isinstance(relative, str) else None
    if (
        relative_path is None
        or relative_path.is_absolute()
        or not relative_path.parts
        or any(part in {"", ".", ".."} for part in relative_path.parts)
        or entry.get("logical_input_id") != f"file:{relative}"
        or not isinstance(entry.get("size_bytes"), int)
        or entry["size_bytes"] < 0
        or not isinstance(entry.get("sha256"), str)
        or len(entry["sha256"]) != 64
        or any(character not in "0123456789abcdef" for character in entry["sha256"])
    ):
        raise SystemExit("retained post-generation evidence inventory entry is invalid")
    if relative in expected:
        raise SystemExit(f"duplicate retained post-generation evidence path: {relative}")
    expected[relative] = (entry["size_bytes"], entry["sha256"])

observed = {}
for directory, directory_names, file_names in os.walk(stage, followlinks=False):
    directory_path = Path(directory)
    for name in directory_names:
        child = directory_path / name
        metadata = child.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
            raise SystemExit(f"retained post-generation evidence has unsafe entry: {child}")
    for name in file_names:
        path = directory_path / name
        relative = path.relative_to(stage).as_posix()
        if relative == "post_generation_evidence_inventory.json":
            continue
        digest, size_bytes, _ = stable_regular_digest(path)
        observed[relative] = (size_bytes, digest)

if observed != expected:
    raise SystemExit(
        "retained post-generation evidence changed during remote consumption: "
        f"missing={sorted(set(expected) - set(observed))}, "
        f"unlisted={sorted(set(observed) - set(expected))}, "
        f"metadata_mismatch={sorted(path for path in set(expected) & set(observed) if expected[path] != observed[path])}"
    )

package_digest = hashlib.sha256()
package_size = 0
for relative, (size_bytes, digest) in sorted(observed.items()):
    package_digest.update(relative.encode("utf-8"))
    package_digest.update(b"\0")
    package_digest.update(str(size_bytes).encode("ascii"))
    package_digest.update(b"\0")
    package_digest.update(digest.encode("ascii"))
    package_digest.update(b"\n")
    package_size += size_bytes
print(
    "|".join(
        (
            inventory_sha256,
            package_digest.hexdigest(),
            str(len(observed)),
            str(package_size),
        )
    )
)
PY
)"; then
  IFS='|' read -r \
    post_generation_inventory_sha256 \
    post_generation_package_sha256 \
    post_generation_package_file_count \
    post_generation_package_size_bytes <<< "$post_generation_package_summary"
  post_generation_package_status="pass"
  log_ok "Post-generation evidence package remained exact after remote consumption"
else
  log_fail "Post-generation evidence package changed or became unsafe during remote consumption"
  post_generation_package_status="fail"
  if [[ "$post_generation_budget_exit" -eq 0 ]]; then
    post_generation_budget_exit=95
  fi
fi

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
if [[ "$post_generation_budget_exit" -eq 0 \
  && "$CARGO_RUNNER_MODE" == "rch" ]] \
  && ! verify_current_clean_source_identity "RCH post-generation budget postcondition"; then
  post_generation_budget_exit=96
fi

post_generation_budget_status="pass"
if [[ "$post_generation_budget_exit" -eq 0 ]]; then
  suite_pass=$((suite_pass + 1))
  log_ok "Post-generation perf budget data-contract evaluation passed"
else
  post_generation_budget_status="fail"
  suite_fail=$((suite_fail + 1))
  log_warn "Post-generation perf budget data-contract evaluation failed (exit=$post_generation_budget_exit)"
fi
else
  post_generation_package_status="skip"
  suite_skip=$((suite_skip + 1))
fi

staging_exit=0
if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" == true ]]; then
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
    if [[ "$ARTIFACT_STAGING_BLOCKERS" -gt 0 ]]; then
      # Name the blocked contracts so the gate transcript explains itself.
      python3 - "$STAGING_MANIFEST_PATH" <<'PY' | while IFS= read -r line; do log_warn "  $line"; done
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
for blocker in payload.get("blockers", [])[:20]:
    if isinstance(blocker, dict):
        print(f"{blocker.get('status', 'blocked')}: {blocker.get('contract_id', '?')}")
PY
    fi
  else
    log_warn "Final artifact staging manifest was not generated"
  fi
else
  ARTIFACT_STAGING_STATUS="skipped"
fi

post_generation_status="skip"
post_generation_result_exit=0
if [[ "$RUN_EXCLUSIVE_POST_GENERATION_GATE" != true ]]; then
  suite_skip=$((suite_skip + 1))
elif [[ "$post_generation_exit" -ne 0 \
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
  post_generation_status="fail"
  suite_fail=$((suite_fail + 1))
else
  post_generation_status="pass"
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
  POST_GENERATION_PACKAGE_STATUS="$post_generation_package_status" \
  POST_GENERATION_PACKAGE_PATH="$POST_GENERATION_STAGE_RELATIVE" \
  POST_GENERATION_INVENTORY_SHA256="$post_generation_inventory_sha256" \
  POST_GENERATION_PACKAGE_SHA256="$post_generation_package_sha256" \
  POST_GENERATION_PACKAGE_FILE_COUNT="$post_generation_package_file_count" \
  POST_GENERATION_PACKAGE_SIZE_BYTES="$post_generation_package_size_bytes" \
  POST_GENERATION_RUN_INSTANCE_ID="$RUN_INSTANCE_ID" \
  POST_GENERATION_SOURCE_COMMIT="$GIT_COMMIT_FULL" \
  POST_GENERATION_SOURCE_DIRTY="$GIT_DIRTY" \
  POST_GENERATION_CORRELATION_ID="$CORRELATION_ID" \
  POST_GENERATION_GATE_SELECTED="$RUN_EXCLUSIVE_POST_GENERATION_GATE" \
  POST_GENERATION_SKIP_REASON="$post_generation_skip_reason" \
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
manifest["post_generation_evidence_package"] = {
    "status": os.environ["POST_GENERATION_PACKAGE_STATUS"],
    "relative_path": os.environ["POST_GENERATION_PACKAGE_PATH"] or None,
    "inventory_sha256": os.environ["POST_GENERATION_INVENTORY_SHA256"] or None,
    "package_sha256": os.environ["POST_GENERATION_PACKAGE_SHA256"] or None,
    "file_count": int(os.environ["POST_GENERATION_PACKAGE_FILE_COUNT"]),
    "size_bytes": int(os.environ["POST_GENERATION_PACKAGE_SIZE_BYTES"]),
    "run_instance_id": os.environ["POST_GENERATION_RUN_INSTANCE_ID"],
    "source_commit": os.environ["POST_GENERATION_SOURCE_COMMIT"],
    "source_dirty": os.environ["POST_GENERATION_SOURCE_DIRTY"] == "true",
    "correlation_id": os.environ["POST_GENERATION_CORRELATION_ID"],
    "exclusive_gate_selected": os.environ["POST_GENERATION_GATE_SELECTED"] == "true",
    "skip_reason": os.environ["POST_GENERATION_SKIP_REASON"] or None,
}
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
status_counts = {"pass": 0, "fail": 0, "skip": 0}
for result in suite_results:
    status = result.get("status") if isinstance(result, dict) else None
    if status in status_counts:
        status_counts[status] += 1
run_summary.update(
    {
        "total_suites": len(suite_results),
        "passed": status_counts["pass"],
        "failed": status_counts["fail"],
        "skipped": status_counts["skip"],
    }
)
manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
PY

log_ok "Total artifacts collected and finalized: $artifact_count"

# ─── Phase 6: Generate checksums ────────────────────────────────────────────

if [[ "$CARGO_RUNNER_MODE" == "rch" ]] \
  && ! verify_current_clean_source_identity "RCH checksum precondition"; then
  die "RCH checksum source identity is not stable"
fi

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

if [[ "$CARGO_RUNNER_MODE" == "rch" ]] \
  && ! verify_current_clean_source_identity "RCH final-success precondition"; then
  die "RCH final-success source identity is not stable"
fi

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
