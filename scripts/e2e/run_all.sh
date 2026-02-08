#!/usr/bin/env bash
# scripts/e2e/run_all.sh — Unified verification runner with structured artifacts.
#
# Runs lint gates, inline lib tests, integration test targets, and E2E suites
# with per-target artifact collection. Targets auto-discovered from
# tests/suite_classification.toml.
#
# Usage:
#   ./scripts/e2e/run_all.sh                              # profile=full (lint + lib + all targets)
#   ./scripts/e2e/run_all.sh --profile quick              # fast local loop (lint + lib + unit only)
#   ./scripts/e2e/run_all.sh --profile focused            # lint + lib + selected integration targets
#   ./scripts/e2e/run_all.sh --profile ci                 # deterministic CI (lint + lib + all non-e2e + 1 e2e)
#   ./scripts/e2e/run_all.sh --suite e2e_tui              # run specific E2E suite(s)
#   ./scripts/e2e/run_all.sh --unit-target node_http_shim # run specific integration target(s)
#   ./scripts/e2e/run_all.sh --rerun-from <summary.json>  # deterministic rerun of failed suites
#   ./scripts/e2e/run_all.sh --diff-from <summary.json>   # compare current run to baseline
#   ./scripts/e2e/run_all.sh --skip-lint                  # skip format/clippy gates
#   ./scripts/e2e/run_all.sh --list                       # list available suites
#   ./scripts/e2e/run_all.sh --list-profiles              # list built-in profiles
#
# Environment:
#   E2E_ARTIFACT_DIR   Override artifact output directory (default: tests/e2e_results/<timestamp>)
#   E2E_PARALLELISM    Cargo test threads (default: 1 for determinism)
#   RUST_LOG           Log level for test harness (default: info)
#   VCR_MODE           Override VCR mode for all suites (default: unset, per-test decision)
#   VERIFY_PROFILE     Default profile when --profile is omitted (default: full)
#   E2E_DIFF_FROM      Baseline summary.json to diff against (optional)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

# ─── Configuration ────────────────────────────────────────────────────────────

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/$TIMESTAMP}"
PARALLELISM="${E2E_PARALLELISM:-1}"
LOG_LEVEL="${RUST_LOG:-info}"
PROFILE="${VERIFY_PROFILE:-full}"
RERUN_FROM=""
DIFF_FROM="${E2E_DIFF_FROM:-}"

# ─── Auto-discover targets from suite_classification.toml ─────────────────────

CLASSIFICATION_FILE="$PROJECT_ROOT/tests/suite_classification.toml"

read_toml_array() {
    local suite_name="$1"
    python3 -c "
import tomllib, sys
with open('$CLASSIFICATION_FILE', 'rb') as f:
    data = tomllib.load(f)
for name in data.get('suite', {}).get('$suite_name', {}).get('files', []):
    print(name)
" 2>/dev/null || true
}

mapfile -t ALL_UNIT_FILES < <(read_toml_array "unit")
mapfile -t ALL_VCR_FILES < <(read_toml_array "vcr")
mapfile -t ALL_E2E_FILES < <(read_toml_array "e2e")

# Combined non-E2E targets (unit + vcr) for integration test phase.
ALL_UNIT_TARGETS=("${ALL_UNIT_FILES[@]}" "${ALL_VCR_FILES[@]}")

# E2E suites.
ALL_SUITES=("${ALL_E2E_FILES[@]}")

# ─── CLI Parsing ──────────────────────────────────────────────────────────────

SELECTED_SUITES=()
SELECTED_UNIT_TARGETS=()
LIST_ONLY=false
LIST_PROFILES=false
SKIP_UNIT=false
SKIP_LINT=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --suite)
            shift
            SELECTED_SUITES+=("$1")
            shift
            ;;
        --unit-target)
            shift
            SELECTED_UNIT_TARGETS+=("$1")
            shift
            ;;
        --profile)
            shift
            PROFILE="$1"
            shift
            ;;
        --rerun-from)
            shift
            RERUN_FROM="$1"
            shift
            ;;
        --diff-from)
            shift
            DIFF_FROM="$1"
            shift
            ;;
        --skip-unit)
            SKIP_UNIT=true
            shift
            ;;
        --skip-lint)
            SKIP_LINT=true
            shift
            ;;
        --list)
            LIST_ONLY=true
            shift
            ;;
        --list-profiles)
            LIST_PROFILES=true
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [--profile NAME] [--suite NAME]... [--unit-target NAME]..."
            echo "          [--rerun-from SUMMARY_JSON] [--diff-from SUMMARY_JSON]"
            echo "          [--skip-unit] [--skip-lint]"
            echo "          [--list] [--list-profiles] [--help]"
            echo ""
            echo "Options:"
            echo "  --profile NAME       Verification profile: quick | focused | ci | full"
            echo "  --suite NAME         Run only specified E2E suite(s) (repeatable)"
            echo "  --unit-target NAME   Run only specified unit target(s) (repeatable)"
            echo "  --rerun-from PATH    Rerun failed suites from prior summary.json"
            echo "  --diff-from PATH     Compare current run against baseline summary.json"
            echo "  --skip-unit          Skip integration target execution"
            echo "  --skip-lint          Skip fmt/clippy lint gates"
            echo "  --list               List available E2E suites and exit"
            echo "  --list-profiles      List available verification profiles and exit"
            echo "  --help               Show this help"
            echo ""
            echo "Profiles:"
            echo "  quick    Lint + lib inline tests + unit suite only (fastest)"
            echo "  focused  Lint + lib + unit + selected integration targets"
            echo "  ci       Lint + lib + all non-E2E targets + 1 E2E suite"
            echo "  full     Lint + lib + all targets + all E2E suites"
            echo ""
            echo "Environment:"
            echo "  E2E_ARTIFACT_DIR     Artifact output directory"
            echo "  E2E_PARALLELISM      Cargo test threads (default: 1)"
            echo "  RUST_LOG             Log level (default: info)"
            echo "  VERIFY_PROFILE       Default profile when --profile not provided"
            echo "  E2E_DIFF_FROM        Baseline summary.json for diff/triage output"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

if $LIST_PROFILES; then
    cat <<EOF
Available verification profiles:
  quick:   Lint + lib inline tests + unit suite targets only (${#ALL_UNIT_FILES[@]} targets)
  focused: Lint + lib + unit + selected integration targets + 2 E2E suites
  ci:      Lint + lib + all non-E2E targets (${#ALL_UNIT_TARGETS[@]}) + 1 E2E suite
  full:    Lint + lib + all targets (${#ALL_UNIT_TARGETS[@]} + ${#ALL_SUITES[@]} E2E)
EOF
    exit 0
fi

if $LIST_ONLY; then
    echo "Available E2E suites (${#ALL_SUITES[@]}):"
    for suite in "${ALL_SUITES[@]}"; do
        if [[ -f "tests/${suite}.rs" ]]; then
            echo "  $suite"
        else
            echo "  $suite (missing)"
        fi
    done
    echo ""
    echo "Non-E2E targets (${#ALL_UNIT_TARGETS[@]}):"
    echo "  Unit suite (${#ALL_UNIT_FILES[@]}): ${ALL_UNIT_FILES[*]:0:5}..."
    echo "  VCR suite (${#ALL_VCR_FILES[@]}): ${ALL_VCR_FILES[*]:0:5}..."
    exit 0
fi

if [[ ${#SELECTED_SUITES[@]} -eq 0 ]]; then
    case "$PROFILE" in
        full)
            SELECTED_SUITES=("${ALL_SUITES[@]}")
            ;;
        focused)
            SELECTED_SUITES=(e2e_extension_registration e2e_tools)
            ;;
        ci)
            SELECTED_SUITES=(e2e_extension_registration)
            ;;
        quick)
            SELECTED_SUITES=()
            ;;
        *)
            echo "Unknown --profile value: $PROFILE (expected: quick|focused|ci|full)" >&2
            exit 1
            ;;
    esac
fi

if [[ ${#SELECTED_UNIT_TARGETS[@]} -eq 0 && "$SKIP_UNIT" == false ]]; then
    case "$PROFILE" in
        full|ci)
            SELECTED_UNIT_TARGETS=("${ALL_UNIT_TARGETS[@]}")
            ;;
        quick)
            SELECTED_UNIT_TARGETS=("${ALL_UNIT_FILES[@]}")
            ;;
        focused)
            SELECTED_UNIT_TARGETS=(
                "${ALL_UNIT_FILES[@]}"
                ext_conformance_matrix
                node_buffer_shim
                node_bun_api_matrix
                node_crypto_shim
                node_http_shim
                npm_module_stubs
                extensions_registration
                extensions_policy_negative
            )
            ;;
    esac
fi

if [[ "$SKIP_UNIT" == true ]]; then
    SELECTED_UNIT_TARGETS=()
fi

if [[ -n "$RERUN_FROM" ]]; then
    if [[ ! -f "$RERUN_FROM" ]]; then
        echo "Rerun summary not found: $RERUN_FROM" >&2
        exit 1
    fi
    mapfile -t rerun_suites < <(python3 - "$RERUN_FROM" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    payload = json.load(handle)
for name in payload.get("failed_names", []):
    if isinstance(name, str) and name:
        print(name)
PY
)

    if [[ ${#rerun_suites[@]} -eq 0 ]]; then
        echo "[rerun] No failed suites found in $RERUN_FROM"
        exit 0
    fi
    SELECTED_SUITES=("${rerun_suites[@]}")
fi

if [[ -z "$DIFF_FROM" && -n "$RERUN_FROM" ]]; then
    DIFF_FROM="$RERUN_FROM"
fi

if [[ -n "$DIFF_FROM" && ! -f "$DIFF_FROM" ]]; then
    echo "Diff baseline summary not found: $DIFF_FROM" >&2
    exit 1
fi

RERUN_JSON_VALUE="null"
if [[ -n "$RERUN_FROM" ]]; then
    RERUN_JSON_VALUE="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$RERUN_FROM")"
fi

DIFF_JSON_VALUE="null"
if [[ -n "$DIFF_FROM" ]]; then
    DIFF_JSON_VALUE="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$DIFF_FROM")"
fi

# ─── Environment Capture ─────────────────────────────────────────────────────

mkdir -p "$ARTIFACT_DIR"

capture_env() {
    local env_file="$ARTIFACT_DIR/environment.json"
    local rustc_version cargo_version os_info git_sha git_branch
    rustc_version="$(rustc --version 2>/dev/null || echo 'unknown')"
    cargo_version="$(cargo --version 2>/dev/null || echo 'unknown')"
    os_info="$(uname -srm 2>/dev/null || echo 'unknown')"
    git_sha="$(git rev-parse --short HEAD 2>/dev/null || echo 'unknown')"
    git_branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo 'unknown')"

    cat > "$env_file" <<ENVJSON
{
  "timestamp": "$TIMESTAMP",
  "profile": "$PROFILE",
  "rerun_from": $RERUN_JSON_VALUE,
  "diff_from": $DIFF_JSON_VALUE,
  "rustc": "$rustc_version",
  "cargo": "$cargo_version",
  "os": "$os_info",
  "git_sha": "$git_sha",
  "git_branch": "$git_branch",
  "parallelism": $PARALLELISM,
  "log_level": "$LOG_LEVEL",
  "artifact_dir": "$ARTIFACT_DIR",
  "vcr_mode": "${VCR_MODE:-unset}",
  "unit_targets": $(printf '%s\n' "${SELECTED_UNIT_TARGETS[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'),
  "e2e_suites": $(printf '%s\n' "${SELECTED_SUITES[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]')
}
ENVJSON
    echo "[env] Captured environment to $env_file"
}

# ─── Lint Gates ──────────────────────────────────────────────────────────────

run_lint_gates() {
    if $SKIP_LINT; then
        echo "[lint] Skipped (--skip-lint)"
        return 0
    fi

    local lint_dir="$ARTIFACT_DIR/lint"
    mkdir -p "$lint_dir"
    local lint_ok=true

    echo "[lint] Running format check..."
    if cargo fmt --check > "$lint_dir/fmt.log" 2>&1; then
        echo "[lint] cargo fmt: PASS"
    else
        echo "[lint] cargo fmt: FAIL (see $lint_dir/fmt.log)"
        lint_ok=false
    fi

    echo "[lint] Running clippy..."
    if cargo clippy --all-targets -- -D warnings > "$lint_dir/clippy.log" 2>&1; then
        echo "[lint] clippy: PASS"
    else
        echo "[lint] clippy: FAIL (see $lint_dir/clippy.log)"
        lint_ok=false
    fi

    if $lint_ok; then
        echo "[lint] All gates passed"
        return 0
    else
        echo "[lint] Some gates failed" >&2
        return 1
    fi
}

# ─── Lib Inline Tests ────────────────────────────────────────────────────────

run_lib_tests() {
    local lib_dir="$ARTIFACT_DIR/lib"
    local log_file="$lib_dir/output.log"
    local result_file="$lib_dir/result.json"
    mkdir -p "$lib_dir"

    echo "[lib] Running inline unit tests (cargo test --lib)..."
    local start_epoch exit_code duration_ms
    start_epoch=$(date +%s%N 2>/dev/null || date +%s)

    set +e
    cargo test --lib -- --test-threads="$PARALLELISM" 2>&1 | tee "$log_file"
    exit_code=${PIPESTATUS[0]}
    set -e

    local end_epoch
    end_epoch=$(date +%s%N 2>/dev/null || date +%s)
    if [[ ${#start_epoch} -gt 12 ]]; then
        duration_ms=$(( (end_epoch - start_epoch) / 1000000 ))
    else
        duration_ms=$(( (end_epoch - start_epoch) * 1000 ))
    fi

    local passed failed ignored total
    passed=$(grep -oP '\d+ passed' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    failed=$(grep -oP '\d+ failed' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    ignored=$(grep -oP '\d+ ignored' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    total=$((passed + failed + ignored))

    cat > "$result_file" <<RESULTJSON
{
  "target": "lib",
  "exit_code": $exit_code,
  "duration_ms": $duration_ms,
  "passed": $passed,
  "failed": $failed,
  "ignored": $ignored,
  "total": $total,
  "log_file": "$log_file",
  "timestamp": "$TIMESTAMP"
}
RESULTJSON

    if [[ $exit_code -eq 0 ]]; then
        echo "[lib] PASS ($passed passed, $ignored ignored, ${duration_ms}ms)"
    else
        echo "[lib] FAIL (exit $exit_code, $failed failed, $passed passed, ${duration_ms}ms)"
    fi

    return $exit_code
}

# ─── Build First ──────────────────────────────────────────────────────────────

build_tests() {
    echo "[build] Compiling selected verification targets..."
    local build_log="$ARTIFACT_DIR/build.log"
    local build_ok=true

    for target in "${SELECTED_UNIT_TARGETS[@]}"; do
        if [[ ! -f "tests/${target}.rs" ]]; then
            echo "[build]   $target (unit target missing, skipping)"
            continue
        fi
        echo "[build]   unit:$target"
        if ! cargo test --test "$target" --no-run 2>>"$build_log"; then
            echo "[build]   unit:$target FAILED" >&2
            build_ok=false
        fi
    done

    for suite in "${SELECTED_SUITES[@]}"; do
        if [[ ! -f "tests/${suite}.rs" ]]; then
            echo "[build]   $suite (suite missing, skipping)"
            continue
        fi
        echo "[build]   e2e:$suite"
        if ! cargo test --test "$suite" --no-run 2>>"$build_log"; then
            echo "[build]   e2e:$suite FAILED" >&2
            build_ok=false
        fi
    done

    if $build_ok; then
        echo "[build] OK"
        return 0
    else
        echo "[build] Some targets failed — see $build_log" >&2
        return 1
    fi
}

# ─── Run a Single Suite ──────────────────────────────────────────────────────

run_unit_target() {
    local target="$1"
    local target_dir="$ARTIFACT_DIR/unit/$target"
    local log_file="$target_dir/output.log"
    local result_file="$target_dir/result.json"
    local start_epoch exit_code duration_ms

    if [[ ! -f "tests/${target}.rs" ]]; then
        echo "[unit] $target: test file not found (tests/${target}.rs)"
        return 1
    fi

    mkdir -p "$target_dir"

    echo "[unit] Running: $target"
    start_epoch=$(date +%s%N 2>/dev/null || date +%s)

    export TEST_LOG_JSONL_PATH="$target_dir/test-log.jsonl"
    export TEST_ARTIFACT_INDEX_PATH="$target_dir/artifact-index.jsonl"
    export RUST_LOG="$LOG_LEVEL"

    set +e
    cargo test \
        --test "$target" \
        -- \
        --test-threads="$PARALLELISM" \
        2>&1 | tee "$log_file"
    exit_code=${PIPESTATUS[0]}
    set -e

    local end_epoch
    end_epoch=$(date +%s%N 2>/dev/null || date +%s)
    if [[ ${#start_epoch} -gt 12 ]]; then
        duration_ms=$(( (end_epoch - start_epoch) / 1000000 ))
    else
        duration_ms=$(( (end_epoch - start_epoch) * 1000 ))
    fi

    local passed failed ignored total
    passed=$(grep -oP '\d+ passed' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    failed=$(grep -oP '\d+ failed' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    ignored=$(grep -oP '\d+ ignored' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    total=$((passed + failed + ignored))

    cat > "$result_file" <<RESULTJSON
{
  "target": "$target",
  "exit_code": $exit_code,
  "duration_ms": $duration_ms,
  "passed": $passed,
  "failed": $failed,
  "ignored": $ignored,
  "total": $total,
  "log_file": "$log_file",
  "timestamp": "$TIMESTAMP"
}
RESULTJSON

    if [[ $exit_code -eq 0 ]]; then
        echo "[unit] $target: PASS ($passed passed, $ignored ignored, ${duration_ms}ms)"
    else
        echo "[unit] $target: FAIL (exit $exit_code, $failed failed, $passed passed, ${duration_ms}ms)"
        echo "[triage] Unit logs: $log_file"
        echo "[triage] Unit artifacts: $target_dir/"
        if [[ -f "$target_dir/test-log.jsonl" ]]; then
            echo "[triage] Unit JSONL log: $target_dir/test-log.jsonl"
        fi
    fi

    return $exit_code
}

run_suite() {
    local suite="$1"
    local suite_dir="$ARTIFACT_DIR/$suite"
    local log_file="$suite_dir/output.log"
    local result_file="$suite_dir/result.json"
    local start_epoch exit_code duration_ms

    mkdir -p "$suite_dir"

    echo "[suite] Running: $suite"

    start_epoch=$(date +%s%N 2>/dev/null || date +%s)

    # Set per-suite environment for test harness logging.
    export TEST_LOG_JSONL_PATH="$suite_dir/test-log.jsonl"
    export TEST_ARTIFACT_INDEX_PATH="$suite_dir/artifact-index.jsonl"
    export RUST_LOG="$LOG_LEVEL"

    set +e
    cargo test \
        --test "$suite" \
        -- \
        --test-threads="$PARALLELISM" \
        2>&1 | tee "$log_file"
    exit_code=${PIPESTATUS[0]}
    set -e

    local end_epoch
    end_epoch=$(date +%s%N 2>/dev/null || date +%s)

    # Compute duration (nanosecond precision if available, else seconds).
    if [[ ${#start_epoch} -gt 12 ]]; then
        duration_ms=$(( (end_epoch - start_epoch) / 1000000 ))
    else
        duration_ms=$(( (end_epoch - start_epoch) * 1000 ))
    fi

    # Parse test counts from cargo test output.
    local passed failed ignored total
    passed=$(grep -oP '\d+ passed' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    failed=$(grep -oP '\d+ failed' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    ignored=$(grep -oP '\d+ ignored' "$log_file" | tail -1 | grep -oP '\d+' || echo "0")
    total=$((passed + failed + ignored))

    cat > "$result_file" <<RESULTJSON
{
  "suite": "$suite",
  "exit_code": $exit_code,
  "duration_ms": $duration_ms,
  "passed": $passed,
  "failed": $failed,
  "ignored": $ignored,
  "total": $total,
  "log_file": "$log_file",
  "timestamp": "$TIMESTAMP"
}
RESULTJSON

    if [[ $exit_code -eq 0 ]]; then
        echo "[suite] $suite: PASS ($passed passed, $ignored ignored, ${duration_ms}ms)"
    else
        echo "[suite] $suite: FAIL (exit $exit_code, $failed failed, $passed passed, ${duration_ms}ms)"
        # Emit failure triage hints.
        echo "[triage] Logs: $log_file"
        echo "[triage] Artifacts: $suite_dir/"
        if [[ -f "$suite_dir/test-log.jsonl" ]]; then
            echo "[triage] JSONL log: $suite_dir/test-log.jsonl"
        fi
    fi

    return $exit_code
}

# ─── Summary Manifest ────────────────────────────────────────────────────────

write_summary() {
    local summary_file="$ARTIFACT_DIR/summary.json"
    local total_units=${#SELECTED_UNIT_TARGETS[@]}
    local passed_units=0
    local failed_units=0
    local failed_unit_names=()
    local total_suites=${#SELECTED_SUITES[@]}
    local passed_suites=0
    local failed_suites=0
    local failed_names=()

    echo "[summary] Writing manifest to $summary_file"

    # Read unit target results.
    local unit_results_array="["
    local first_unit=true
    for target in "${SELECTED_UNIT_TARGETS[@]}"; do
        local result_file="$ARTIFACT_DIR/unit/$target/result.json"
        if [[ -f "$result_file" ]]; then
            local exit_code
            exit_code=$(python3 -c "import json; print(json.load(open('$result_file'))['exit_code'])" 2>/dev/null || echo "1")
            if [[ "$exit_code" -eq 0 ]]; then
                ((passed_units++)) || true
            else
                ((failed_units++)) || true
                failed_unit_names+=("$target")
            fi
            if ! $first_unit; then unit_results_array+=","; fi
            unit_results_array+="$(cat "$result_file")"
            first_unit=false
        else
            ((failed_units++)) || true
            failed_unit_names+=("$target")
            if ! $first_unit; then unit_results_array+=","; fi
            unit_results_array+="{\"target\":\"$target\",\"exit_code\":1,\"error\":\"no result file\"}"
            first_unit=false
        fi
    done
    unit_results_array+="]"

    # Read E2E suite results.
    local suite_results_array="["
    local first_suite=true
    for suite in "${SELECTED_SUITES[@]}"; do
        local result_file="$ARTIFACT_DIR/$suite/result.json"
        if [[ -f "$result_file" ]]; then
            local exit_code
            exit_code=$(python3 -c "import json; print(json.load(open('$result_file'))['exit_code'])" 2>/dev/null || echo "1")
            if [[ "$exit_code" -eq 0 ]]; then
                ((passed_suites++)) || true
            else
                ((failed_suites++)) || true
                failed_names+=("$suite")
            fi
            if ! $first_suite; then suite_results_array+=","; fi
            suite_results_array+="$(cat "$result_file")"
            first_suite=false
        else
            ((failed_suites++)) || true
            failed_names+=("$suite")
            if ! $first_suite; then suite_results_array+=","; fi
            suite_results_array+="{\"suite\":\"$suite\",\"exit_code\":1,\"error\":\"no result file\"}"
            first_suite=false
        fi
    done
    suite_results_array+="]"

    # Redact secrets from logs.
    redact_secrets

    cat > "$summary_file" <<SUMMARYJSON
{
  "timestamp": "$TIMESTAMP",
  "profile": "$PROFILE",
  "rerun_from": $RERUN_JSON_VALUE,
  "diff_from": $DIFF_JSON_VALUE,
  "artifact_dir": "$ARTIFACT_DIR",
  "total_units": $total_units,
  "passed_units": $passed_units,
  "failed_units": $failed_units,
  "failed_unit_names": $(printf '%s\n' "${failed_unit_names[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'),
  "total_suites": $total_suites,
  "passed_suites": $passed_suites,
  "failed_suites": $failed_suites,
  "failed_names": $(printf '%s\n' "${failed_names[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'),
  "unit_targets": $unit_results_array,
  "suites": $suite_results_array
}
SUMMARYJSON

    # Lint status.
    local lint_status="skip"
    if ! $SKIP_LINT; then
        if [[ -f "$ARTIFACT_DIR/lint/fmt.log" ]] && [[ -f "$ARTIFACT_DIR/lint/clippy.log" ]]; then
            local fmt_ok clippy_ok
            fmt_ok=$([[ $(wc -c < "$ARTIFACT_DIR/lint/fmt.log") -eq 0 ]] && echo "pass" || echo "fail")
            clippy_ok=$(grep -q "error" "$ARTIFACT_DIR/lint/clippy.log" 2>/dev/null && echo "fail" || echo "pass")
            lint_status="${fmt_ok}/${clippy_ok}"
        else
            lint_status="not_run"
        fi
    fi

    # Lib test status.
    local lib_status="not_run"
    if [[ -f "$ARTIFACT_DIR/lib/result.json" ]]; then
        local lib_exit
        lib_exit=$(python3 -c "import json; print(json.load(open('$ARTIFACT_DIR/lib/result.json'))['exit_code'])" 2>/dev/null || echo "1")
        lib_status=$( [[ "$lib_exit" -eq 0 ]] && echo "pass" || echo "fail" )
    fi

    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo " Verification Summary (profile: $PROFILE)"
    echo " Lint gates:     $lint_status"
    echo " Lib inline:     $lib_status"
    echo " Integration:    $passed_units/$total_units passed"
    echo " E2E suites:     $passed_suites/$total_suites passed"
    if [[ $failed_units -gt 0 ]]; then
        echo " Failed targets: ${failed_unit_names[*]}"
    fi
    if [[ $failed_suites -gt 0 ]]; then
        echo " Failed E2E:     ${failed_names[*]}"
    fi
    echo " Artifacts: $ARTIFACT_DIR"
    echo "═══════════════════════════════════════════════════════════════"
}

# ─── Capability Profile Matrix (bd-k5q5.7.5) ────────────────────────────────

generate_extension_profile_matrix() {
    local matrix_file="$ARTIFACT_DIR/extension_profile_matrix.json"
    local matrix_markdown="$ARTIFACT_DIR/extension_profile_matrix.md"
    local summary_file="$ARTIFACT_DIR/summary.json"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        PROJECT_ROOT="$PROJECT_ROOT" \
        MATRIX_FILE="$matrix_file" \
        MATRIX_MARKDOWN="$matrix_markdown" \
        SUMMARY_FILE="$summary_file" \
        python3 - <<'PY'
import json
import os
import subprocess
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
matrix_file = Path(os.environ["MATRIX_FILE"])
matrix_markdown = Path(os.environ["MATRIX_MARKDOWN"])
summary_file = Path(os.environ["SUMMARY_FILE"])

conformance_events_path = (
    project_root / "tests" / "ext_conformance" / "reports" / "conformance_events.jsonl"
)
negative_events_path = (
    project_root / "tests" / "ext_conformance" / "reports" / "negative" / "negative_events.jsonl"
)

for path in (conformance_events_path, negative_events_path):
    if not path.exists():
        raise SystemExit(f"[profiles] required input missing: {path}")


def load_jsonl(path: Path) -> list[dict]:
    rows: list[dict] = []
    with path.open(encoding="utf-8") as handle:
        for line_no, raw in enumerate(handle, start=1):
            text = raw.strip()
            if not text:
                continue
            try:
                payload = json.loads(text)
            except json.JSONDecodeError as exc:
                raise SystemExit(f"[profiles] invalid JSONL in {path} line {line_no}: {exc}") from exc
            if isinstance(payload, dict):
                rows.append(payload)
    return rows


def explain_policy(profile: str) -> dict:
    command = [
        "cargo",
        "run",
        "--quiet",
        "--bin",
        "pi",
        "--",
        "--explain-extension-policy",
        "--extension-policy",
        profile,
    ]
    result = subprocess.run(
        command,
        cwd=project_root,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(
            "[profiles] failed to query policy "
            f"for profile={profile}: rc={result.returncode}\n{result.stderr}"
        )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise SystemExit(
            f"[profiles] explain-policy output is not JSON for profile={profile}: {exc}"
        ) from exc
    if not isinstance(payload, dict):
        raise SystemExit(f"[profiles] explain-policy payload is not an object for profile={profile}")
    return payload


def infer_capabilities(event: dict) -> list[str]:
    caps = set()
    raw_caps = event.get("capabilities", {})
    if not isinstance(raw_caps, dict):
        return []
    if raw_caps.get("uses_exec") is True:
        caps.add("exec")
    if raw_caps.get("uses_http") is True:
        caps.add("http")
    if raw_caps.get("uses_ui") is True:
        caps.add("ui")
    if raw_caps.get("uses_session") is True:
        caps.add("session")
    if raw_caps.get("registers_tools") is True:
        caps.add("tool")
    subscribers = raw_caps.get("subscribes_events")
    if isinstance(subscribers, list) and any(isinstance(item, str) and item for item in subscribers):
        caps.add("events")
    return sorted(caps)


conformance_events = load_jsonl(conformance_events_path)
negative_events = load_jsonl(negative_events_path)

profiles = ["safe", "balanced", "permissive"]
mode_for_profile = {
    "safe": "strict",
    "balanced": "prompt",
    "permissive": "permissive",
}

policy_payloads: dict[str, dict] = {}
policy_decisions: dict[str, dict[str, str]] = {}

for profile in profiles:
    payload = explain_policy(profile)
    policy_payloads[profile] = payload
    decisions: dict[str, str] = {}
    for entry in payload.get("capability_decisions", []):
        if not isinstance(entry, dict):
            continue
        capability = entry.get("capability")
        decision = entry.get("decision")
        if isinstance(capability, str) and capability and isinstance(decision, str) and decision:
            decisions[capability] = decision.lower()
    policy_decisions[profile] = decisions

testable_events: list[dict] = []
for event in conformance_events:
    status = str(event.get("overall_status", "")).upper()
    if status in {"PASS", "FAIL"}:
        testable_events.append(event)

profile_rows: dict[str, dict] = {}

for profile in profiles:
    decisions = policy_decisions[profile]
    decision_counts = Counter(decisions.values())

    projected_pass = 0
    projected_fail_existing = 0
    projected_blocked = 0
    blocked_by_capability: Counter[str] = Counter()
    blocked_examples: list[dict] = []

    for event in testable_events:
        extension_id = str(event.get("extension_id", "unknown"))
        overall_status = str(event.get("overall_status", "")).upper()
        inferred = infer_capabilities(event)
        denied_caps = [cap for cap in inferred if decisions.get(cap) == "deny"]

        if denied_caps:
            projected_blocked += 1
            for cap in denied_caps:
                blocked_by_capability[cap] += 1
            if len(blocked_examples) < 50:
                blocked_examples.append(
                    {
                        "extension_id": extension_id,
                        "denied_capabilities": denied_caps,
                        "baseline_status": overall_status,
                    }
                )
        elif overall_status == "PASS":
            projected_pass += 1
        else:
            projected_fail_existing += 1

    projected_total = projected_pass + projected_fail_existing + projected_blocked
    projected_pass_rate_pct = (
        round((projected_pass / projected_total) * 100.0, 2) if projected_total else 0.0
    )

    profile_rows[profile] = {
        "requested_profile": payload.get("requested_profile"),
        "effective_profile": payload.get("effective_profile"),
        "profile_source": payload.get("profile_source"),
        "decision_counts": {
            "allow": int(decision_counts.get("allow", 0)),
            "prompt": int(decision_counts.get("prompt", 0)),
            "deny": int(decision_counts.get("deny", 0)),
        },
        "projected": {
            "testable_extensions": projected_total,
            "pass": projected_pass,
            "fail_existing": projected_fail_existing,
            "blocked": projected_blocked,
            "pass_rate_pct": projected_pass_rate_pct,
        },
        "blocked_operation_summary": {
            "by_capability": dict(sorted(blocked_by_capability.items())),
            "blocked_extension_count": projected_blocked,
            "blocked_extension_examples": blocked_examples,
        },
    }

checked = 0
matched = 0
mismatches: list[dict] = []
skipped_unknown: Counter[str] = Counter()
skipped_fixture_override: Counter[str] = Counter()

for profile in profiles:
    mode = mode_for_profile[profile]
    decisions = policy_decisions[profile]
    for event in negative_events:
        if str(event.get("mode", "")) != mode:
            continue
        capability = str(event.get("capability", "")).strip()
        if not capability:
            continue
        runtime_decision = str(event.get("actual_decision", "")).strip().lower()
        if not runtime_decision:
            continue

        # The negative-conformance fixture intentionally enforces a global deny-list
        # for dangerous capabilities in permissive mode. That override context is
        # stricter than CLI preflight output and should not be flagged as mismatch.
        if profile == "permissive" and capability in {"exec", "env"}:
            skipped_fixture_override[capability] += 1
            continue

        preflight_decision = decisions.get(capability)
        if preflight_decision is None:
            skipped_unknown[capability] += 1
            continue

        checked += 1
        if preflight_decision == runtime_decision:
            matched += 1
            continue
        mismatches.append(
            {
                "profile": profile,
                "mode": mode,
                "capability": capability,
                "preflight_decision": preflight_decision,
                "runtime_decision": runtime_decision,
                "reason": event.get("reason"),
                "test_name": event.get("test_name"),
            }
        )

payload = {
    "schema": "pi.e2e.extension_profile_matrix.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "source_files": {
        "conformance_events": str(conformance_events_path),
        "negative_events": str(negative_events_path),
    },
    "profiles": profile_rows,
    "preflight_runtime_consistency": {
        "checked_capability_decisions": checked,
        "matched_capability_decisions": matched,
        "mismatched_capability_decisions": len(mismatches),
        "mismatches": mismatches,
        "skipped_unknown_capabilities": dict(sorted(skipped_unknown.items())),
        "skipped_fixture_overrides": dict(sorted(skipped_fixture_override.items())),
    },
}

matrix_file.parent.mkdir(parents=True, exist_ok=True)
matrix_file.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

markdown_lines = [
    "# Extension Capability Profile Matrix",
    "",
    f"- Generated at: `{payload['generated_at']}`",
    f"- Conformance source: `{conformance_events_path}`",
    f"- Runtime policy source: `{negative_events_path}`",
    "",
    "## Profile Projection",
    "",
    "| Profile | Pass | Fail (existing) | Blocked | Pass rate | Deny decisions | Prompt decisions | Allow decisions |",
    "|---|---:|---:|---:|---:|---:|---:|---:|",
]

for profile in profiles:
    row = profile_rows[profile]
    projected = row["projected"]
    counts = row["decision_counts"]
    markdown_lines.append(
        "| "
        f"{profile} | "
        f"{projected['pass']} | "
        f"{projected['fail_existing']} | "
        f"{projected['blocked']} | "
        f"{projected['pass_rate_pct']:.2f}% | "
        f"{counts['deny']} | "
        f"{counts['prompt']} | "
        f"{counts['allow']} |"
    )

markdown_lines.extend(
    [
        "",
        "## Preflight vs Runtime Consistency",
        "",
        f"- Checked decisions: `{checked}`",
        f"- Matched decisions: `{matched}`",
        f"- Mismatches: `{len(mismatches)}`",
        "",
    ]
)
if mismatches:
    markdown_lines.append("### Mismatches")
    markdown_lines.append("")
    for mismatch in mismatches:
        markdown_lines.append(
            "- "
            f"`{mismatch['profile']}` `{mismatch['capability']}`: "
            f"preflight={mismatch['preflight_decision']} runtime={mismatch['runtime_decision']} "
            f"(test `{mismatch['test_name']}`)"
        )
    markdown_lines.append("")

if skipped_unknown:
    markdown_lines.append("### Skipped Unknown Capabilities")
    markdown_lines.append("")
    for capability, count in sorted(skipped_unknown.items()):
        markdown_lines.append(f"- `{capability}`: {count}")
    markdown_lines.append("")

if skipped_fixture_override:
    markdown_lines.append("### Skipped Fixture Overrides")
    markdown_lines.append("")
    for capability, count in sorted(skipped_fixture_override.items()):
        markdown_lines.append(f"- `{capability}`: {count} (negative fixture explicit deny override)")
    markdown_lines.append("")

matrix_markdown.write_text("\n".join(markdown_lines) + "\n", encoding="utf-8")

if summary_file.exists():
    try:
        summary_payload = json.loads(summary_file.read_text(encoding="utf-8"))
        if isinstance(summary_payload, dict):
            summary_payload["extension_profile_matrix"] = {
                "schema": "pi.e2e.extension_profile_matrix.v1",
                "path": str(matrix_file),
                "markdown_path": str(matrix_markdown),
                "preflight_runtime_mismatches": len(mismatches),
                "checked_capability_decisions": checked,
                "profiles": {
                    profile: {
                        "projected_pass_rate_pct": profile_rows[profile]["projected"]["pass_rate_pct"],
                        "blocked_extensions": profile_rows[profile]["projected"]["blocked"],
                    }
                    for profile in profiles
                },
            }
            summary_file.write_text(json.dumps(summary_payload, indent=2) + "\n", encoding="utf-8")
    except Exception as exc:
        raise SystemExit(f"[profiles] failed to enrich summary.json: {exc}") from exc

print("PROFILE MATRIX GENERATED")
print(f"- JSON: {matrix_file}")
print(f"- Markdown: {matrix_markdown}")
print(
    "- Preflight/runtime consistency: "
    f"checked={checked}, matched={matched}, mismatches={len(mismatches)}"
)
for profile in profiles:
    projected = profile_rows[profile]["projected"]
    print(
        f"- {profile}: pass={projected['pass']} "
        f"fail_existing={projected['fail_existing']} blocked={projected['blocked']} "
        f"pass_rate={projected['pass_rate_pct']:.2f}%"
    )
PY
    then
        echo "[profiles] Capability profile matrix generated ($matrix_file)"
        return 0
    else
        echo "[profiles] Failed to generate capability profile matrix" >&2
        return 1
    fi
}

# ─── Soak Longevity Evidence (bd-k5q5.7.10) ──────────────────────────────────

generate_soak_longevity_report() {
    local soak_file="$ARTIFACT_DIR/soak_longevity_report.json"
    local soak_markdown="$ARTIFACT_DIR/soak_longevity_report.md"
    local soak_events="$ARTIFACT_DIR/soak_longevity_events.jsonl"
    local summary_file="$ARTIFACT_DIR/summary.json"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        PROJECT_ROOT="$PROJECT_ROOT" \
        SOAK_FILE="$soak_file" \
        SOAK_MARKDOWN="$soak_markdown" \
        SOAK_EVENTS="$soak_events" \
        SUMMARY_FILE="$summary_file" \
        python3 - <<'PY'
import csv
import json
import os
from datetime import datetime, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
soak_file = Path(os.environ["SOAK_FILE"])
soak_markdown = Path(os.environ["SOAK_MARKDOWN"])
soak_events = Path(os.environ["SOAK_EVENTS"])
summary_file = Path(os.environ["SUMMARY_FILE"])

perf_dir = project_root / "target" / "perf"
mem_report_path = perf_dir / "ext_memory_stress_report.json"
mem_csv_path = perf_dir / "ext_memory_stress.csv"
stress_triage_path = perf_dir / "stress_triage.json"
stress_events_path = perf_dir / "stress_events.jsonl"
profile_rotation_path = perf_dir / "stress_profile_rotation.json"


def read_json(path: Path):
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def as_float(value):
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def as_int(value):
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def ratio(numerator, denominator):
    if numerator is None or denominator in (None, 0):
        return None
    return float(numerator) / float(denominator)


def slope(points):
    if len(points) < 2:
        return None
    first_t, first_v = points[0]
    last_t, last_v = points[-1]
    delta_t = last_t - first_t
    if delta_t <= 0:
        return None
    return (last_v - first_v) / delta_t


summary_payload = read_json(summary_file) or {}
unit_status = {}
for target in summary_payload.get("unit_targets", []):
    if not isinstance(target, dict):
        continue
    name = target.get("target")
    if not isinstance(name, str) or not name:
        continue
    unit_status[name] = target.get("exit_code") == 0

required_targets = ["ext_memory_stress", "extensions_stress"]
missing_targets = [name for name in required_targets if name not in unit_status]
failed_targets = [name for name in required_targets if unit_status.get(name) is False]
required_targets_executed = not missing_targets and not failed_targets

mem_report = read_json(mem_report_path)
stress_triage = read_json(stress_triage_path)
profile_rotation = read_json(profile_rotation_path)

rss_points = []
heap_points = []
if mem_csv_path.exists():
    with mem_csv_path.open(encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle)
        for row in reader:
            elapsed = as_float(row.get("elapsed_secs"))
            rss_kb = as_float(row.get("rss_kb"))
            heap_kb = as_float(row.get("quickjs_heap_kb"))
            if elapsed is None:
                continue
            if rss_kb is not None:
                rss_points.append((elapsed, rss_kb))
            if heap_kb is not None:
                heap_points.append((elapsed, heap_kb))

memory_metrics = {
    "rss_baseline_kb": None,
    "rss_peak_kb": None,
    "rss_growth_factor": None,
    "rss_slope_kb_per_sec": slope(rss_points),
    "monotonic_rss_growth": None,
    "quickjs_baseline_kb": None,
    "quickjs_peak_kb": None,
    "quickjs_growth_factor": None,
    "quickjs_slope_kb_per_sec": slope(heap_points),
    "monotonic_quickjs_growth": None,
    "event_errors": None,
    "events_dispatched": None,
    "error_rate_pct": None,
}

if isinstance(mem_report, dict):
    memory_metrics["rss_baseline_kb"] = as_int(mem_report.get("rss_baseline_kb"))
    memory_metrics["rss_peak_kb"] = as_int(mem_report.get("rss_peak_kb"))
    memory_metrics["rss_growth_factor"] = as_float(mem_report.get("rss_growth_factor"))
    memory_metrics["monotonic_rss_growth"] = bool(mem_report.get("monotonic_rss_growth"))
    memory_metrics["quickjs_baseline_kb"] = as_int(mem_report.get("quickjs_baseline_kb"))
    memory_metrics["quickjs_peak_kb"] = as_int(mem_report.get("quickjs_peak_kb"))
    memory_metrics["quickjs_growth_factor"] = as_float(mem_report.get("quickjs_growth_factor"))
    memory_metrics["monotonic_quickjs_growth"] = bool(
        mem_report.get("monotonic_quickjs_growth")
    )
    memory_metrics["event_errors"] = as_int(mem_report.get("event_errors")) or 0
    memory_metrics["events_dispatched"] = as_int(mem_report.get("events_dispatched")) or 0
    if memory_metrics["events_dispatched"] > 0:
        memory_metrics["error_rate_pct"] = round(
            (memory_metrics["event_errors"] / memory_metrics["events_dispatched"]) * 100.0,
            4,
        )

triage_results = {}
if isinstance(stress_triage, dict):
    triage_results = stress_triage.get("results", {})
if not isinstance(triage_results, dict):
    triage_results = {}

triage_latency = triage_results.get("latency", {})
if not isinstance(triage_latency, dict):
    triage_latency = {}
triage_rss = triage_results.get("rss", {})
if not isinstance(triage_rss, dict):
    triage_rss = {}

triage_event_count = as_int(triage_results.get("event_count")) or 0
triage_error_count = as_int(triage_results.get("error_count")) or 0
triage_error_rate_pct = (
    round((triage_error_count / triage_event_count) * 100.0, 4)
    if triage_event_count > 0
    else None
)

timing_metrics = {
    "p99_first_us": as_int(triage_latency.get("p99_first_us")),
    "p99_last_us": as_int(triage_latency.get("p99_last_us")),
    "latency_degradation_ratio": ratio(
        as_float(triage_latency.get("p99_last_us")),
        as_float(triage_latency.get("p99_first_us")),
    ),
    "latency_ok": bool(triage_latency.get("ok")) if triage_latency else None,
    "events_dispatched": triage_event_count,
    "error_count": triage_error_count,
    "error_rate_pct": triage_error_rate_pct,
    "rss_growth_pct": as_float(triage_rss.get("growth_pct")),
    "rss_ok": bool(triage_rss.get("ok")) if triage_rss else None,
}

profile_rotation_metrics = {
    "present": isinstance(profile_rotation, dict),
    "overall_pass": None,
    "slice_count": 0,
    "profiles": [],
}
if isinstance(profile_rotation, dict):
    slices = profile_rotation.get("slices", [])
    if not isinstance(slices, list):
        slices = []
    profile_rotation_metrics["overall_pass"] = bool(profile_rotation.get("overall_pass"))
    profile_rotation_metrics["slice_count"] = len(slices)
    profile_rotation_metrics["profiles"] = [
        str(slice_payload.get("profile"))
        for slice_payload in slices
        if isinstance(slice_payload, dict) and slice_payload.get("profile")
    ]

thresholds = {
    "rss_growth_factor_max": 2.0,
    "quickjs_growth_factor_max": 2.0,
    "latency_degradation_ratio_max": 2.0,
    "p99_last_us_max": 25_000,
    "error_rate_pct_max": 25.0,
    "profile_rotation_required": True,
}

checks = []
failed_checks = []


def add_check(check_id, ok, actual, threshold, required=True):
    checks.append(
        {
            "id": check_id,
            "ok": bool(ok),
            "actual": actual,
            "threshold": threshold,
            "required": required,
        }
    )
    if required and not ok:
        failed_checks.append(check_id)


add_check(
    "prerequisites.required_targets_executed",
    required_targets_executed,
    {
        "missing_targets": missing_targets,
        "failed_targets": failed_targets,
    },
    "ext_memory_stress + extensions_stress exit_code=0",
)

add_check(
    "inputs.ext_memory_stress_report_present",
    isinstance(mem_report, dict),
    str(mem_report_path),
    "report must exist and parse as JSON",
)
add_check(
    "inputs.extensions_stress_triage_present",
    isinstance(stress_triage, dict),
    str(stress_triage_path),
    "report must exist and parse as JSON",
)
add_check(
    "inputs.profile_rotation_report_present",
    isinstance(profile_rotation, dict),
    str(profile_rotation_path),
    "report must exist and parse as JSON",
)

rss_growth_factor = memory_metrics["rss_growth_factor"]
add_check(
    "memory.rss_growth_factor",
    rss_growth_factor is not None and rss_growth_factor <= thresholds["rss_growth_factor_max"],
    rss_growth_factor,
    f"<= {thresholds['rss_growth_factor_max']}",
)

quickjs_growth_factor = memory_metrics["quickjs_growth_factor"]
add_check(
    "memory.quickjs_growth_factor",
    quickjs_growth_factor is not None
    and quickjs_growth_factor <= thresholds["quickjs_growth_factor_max"],
    quickjs_growth_factor,
    f"<= {thresholds['quickjs_growth_factor_max']}",
)

add_check(
    "memory.no_monotonic_rss_growth",
    memory_metrics["monotonic_rss_growth"] is False,
    memory_metrics["monotonic_rss_growth"],
    "must be false",
)
add_check(
    "memory.no_monotonic_quickjs_growth",
    memory_metrics["monotonic_quickjs_growth"] is False,
    memory_metrics["monotonic_quickjs_growth"],
    "must be false",
)

latency_ratio = timing_metrics["latency_degradation_ratio"]
p99_last_us = timing_metrics["p99_last_us"]
add_check(
    "timing.latency_degradation_ratio",
    (
        latency_ratio is not None
        and (
            latency_ratio <= thresholds["latency_degradation_ratio_max"]
            or (p99_last_us is not None and p99_last_us <= thresholds["p99_last_us_max"])
        )
    ),
    latency_ratio,
    (
        f"<= {thresholds['latency_degradation_ratio_max']} "
        f"or p99_last_us <= {thresholds['p99_last_us_max']}"
    ),
)

triage_error_rate = timing_metrics["error_rate_pct"]
add_check(
    "errors.extensions_stress_error_rate_pct",
    triage_error_rate is not None and triage_error_rate <= thresholds["error_rate_pct_max"],
    triage_error_rate,
    f"<= {thresholds['error_rate_pct_max']}",
)

mem_error_rate = memory_metrics["error_rate_pct"]
add_check(
    "errors.ext_memory_stress_error_rate_pct",
    mem_error_rate is not None and mem_error_rate <= thresholds["error_rate_pct_max"],
    mem_error_rate,
    f"<= {thresholds['error_rate_pct_max']}",
)

add_check(
    "profile_rotation.overall_pass",
    profile_rotation_metrics["overall_pass"] is True,
    profile_rotation_metrics["overall_pass"],
    "must be true",
)

overall_pass = len(failed_checks) == 0
if not required_targets_executed:
    status = "missing_prerequisites"
elif overall_pass:
    status = "pass"
else:
    status = "threshold_failure"

events_payloads = []
if stress_events_path.exists():
    with stress_events_path.open(encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                continue
            events_payloads.append(
                {
                    "schema": "pi.e2e.soak_longevity_event.v1",
                    "source": "extensions_stress",
                    "event_type": payload.get("schema", "unknown"),
                    "payload": payload,
                }
            )

for elapsed, rss_kb in rss_points:
    quickjs_kb = None
    for t_heap, heap_kb in heap_points:
        if t_heap == elapsed:
            quickjs_kb = heap_kb
            break
    events_payloads.append(
        {
            "schema": "pi.e2e.soak_longevity_event.v1",
            "source": "ext_memory_stress",
            "event_type": "pi.ext.memory_stress.sample.v1",
            "payload": {
                "elapsed_secs": elapsed,
                "rss_kb": rss_kb,
                "quickjs_heap_kb": quickjs_kb,
            },
        }
    )

events_payloads.append(
    {
        "schema": "pi.e2e.soak_longevity_event.v1",
        "source": "soak_longevity",
        "event_type": "pi.e2e.soak_longevity.summary.v1",
        "payload": {
            "status": status,
            "pass": overall_pass,
            "failed_checks": failed_checks,
            "generated_at": datetime.now(timezone.utc).isoformat(),
        },
    }
)

soak_events.parent.mkdir(parents=True, exist_ok=True)
with soak_events.open("w", encoding="utf-8") as handle:
    for payload in events_payloads:
        handle.write(json.dumps(payload, separators=(",", ":")) + "\n")

report_payload = {
    "schema": "pi.e2e.soak_longevity.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "status": status,
    "pass": overall_pass,
    "prerequisites": {
        "required_targets": required_targets,
        "required_targets_executed": required_targets_executed,
        "missing_targets": missing_targets,
        "failed_targets": failed_targets,
    },
    "source_files": {
        "ext_memory_stress_report": str(mem_report_path),
        "ext_memory_stress_csv": str(mem_csv_path),
        "extensions_stress_triage": str(stress_triage_path),
        "extensions_stress_events": str(stress_events_path),
        "profile_rotation_report": str(profile_rotation_path),
    },
    "metrics": {
        "memory": memory_metrics,
        "timing": timing_metrics,
        "profile_rotation": profile_rotation_metrics,
    },
    "thresholds": thresholds,
    "checks": checks,
    "failed_checks": failed_checks,
}

soak_file.parent.mkdir(parents=True, exist_ok=True)
soak_file.write_text(json.dumps(report_payload, indent=2) + "\n", encoding="utf-8")

markdown_lines = [
    "# Soak Longevity Verification",
    "",
    f"- Generated at: `{report_payload['generated_at']}`",
    f"- Status: `{status}`",
    f"- Overall pass: `{overall_pass}`",
    "",
    "## Key Metrics",
    "",
    "| Metric | Value |",
    "|---|---:|",
    f"| RSS growth factor | {memory_metrics['rss_growth_factor']} |",
    f"| QuickJS growth factor | {memory_metrics['quickjs_growth_factor']} |",
    f"| Latency degradation ratio (p99 last / first) | {timing_metrics['latency_degradation_ratio']} |",
    f"| extensions_stress error rate (%) | {timing_metrics['error_rate_pct']} |",
    f"| ext_memory_stress error rate (%) | {memory_metrics['error_rate_pct']} |",
    f"| Profile rotation pass | {profile_rotation_metrics['overall_pass']} |",
    "",
    "## Failed Checks",
    "",
]
if failed_checks:
    markdown_lines.extend([f"- `{check_id}`" for check_id in failed_checks])
else:
    markdown_lines.append("- None")
markdown_lines.append("")
markdown_lines.append(f"- Events: `{soak_events}`")
soak_markdown.write_text("\n".join(markdown_lines) + "\n", encoding="utf-8")

if isinstance(summary_payload, dict):
    summary_payload["soak_longevity"] = {
        "schema": "pi.e2e.soak_longevity.v1",
        "path": str(soak_file),
        "markdown_path": str(soak_markdown),
        "events_path": str(soak_events),
        "status": status,
        "pass": overall_pass,
        "failed_checks": failed_checks,
    }
    summary_file.write_text(json.dumps(summary_payload, indent=2) + "\n", encoding="utf-8")

print("SOAK LONGEVITY REPORT GENERATED")
print(f"- JSON: {soak_file}")
print(f"- Markdown: {soak_markdown}")
print(f"- Events: {soak_events}")
print(f"- Status: {status}")
print(f"- Failed checks: {len(failed_checks)}")
PY
    then
        echo "[soak] Soak longevity report generated ($soak_file)"
        return 0
    else
        echo "[soak] Failed to generate soak longevity report" >&2
        return 1
    fi
}

# ─── User-Focused Release Readiness Summary (bd-k5q5.7.11) ──────────────────

generate_release_readiness_report() {
    local readiness_file="$ARTIFACT_DIR/release_readiness_summary.json"
    local readiness_markdown="$ARTIFACT_DIR/release_readiness_summary.md"
    local summary_file="$ARTIFACT_DIR/summary.json"
    local profile_matrix_file="$ARTIFACT_DIR/extension_profile_matrix.json"
    local soak_file="$ARTIFACT_DIR/soak_longevity_report.json"
    local conformance_summary_file="$PROJECT_ROOT/tests/ext_conformance/reports/conformance_summary.json"
    local conformance_report_md="$PROJECT_ROOT/tests/ext_conformance/reports/CONFORMANCE_REPORT.md"
    local contract_file="$ARTIFACT_DIR/evidence_contract.json"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        READINESS_FILE="$readiness_file" \
        READINESS_MARKDOWN="$readiness_markdown" \
        SUMMARY_FILE="$summary_file" \
        PROFILE_MATRIX_FILE="$profile_matrix_file" \
        SOAK_FILE="$soak_file" \
        CONFORMANCE_SUMMARY_FILE="$conformance_summary_file" \
        CONFORMANCE_REPORT_MD="$conformance_report_md" \
        CONTRACT_FILE="$contract_file" \
        python3 - <<'PY'
import json
import os
from datetime import datetime, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
readiness_file = Path(os.environ["READINESS_FILE"])
readiness_markdown = Path(os.environ["READINESS_MARKDOWN"])
summary_file = Path(os.environ["SUMMARY_FILE"])
profile_matrix_file = Path(os.environ["PROFILE_MATRIX_FILE"])
soak_file = Path(os.environ["SOAK_FILE"])
conformance_summary_file = Path(os.environ["CONFORMANCE_SUMMARY_FILE"])
conformance_report_md = Path(os.environ["CONFORMANCE_REPORT_MD"])
contract_file = Path(os.environ["CONTRACT_FILE"])


def read_json(path: Path):
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def as_int(value, default=0):
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def as_float(value, default=0.0):
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


summary = read_json(summary_file) or {}
profile_matrix = read_json(profile_matrix_file) or {}
soak_report = read_json(soak_file) or {}
conformance_summary = read_json(conformance_summary_file) or {}
contract_payload = read_json(contract_file)

counts = conformance_summary.get("counts", {}) if isinstance(conformance_summary, dict) else {}
conformance_total = as_int(counts.get("total"))
conformance_pass = as_int(counts.get("pass"))
conformance_fail = as_int(counts.get("fail"))
conformance_na = as_int(counts.get("na"))
conformance_pass_rate = as_float(conformance_summary.get("pass_rate_pct"))

failed_units = as_int(summary.get("failed_units"))
failed_suites = as_int(summary.get("failed_suites"))
total_units = as_int(summary.get("total_units"))
total_suites = as_int(summary.get("total_suites"))

suite_entries = summary.get("suites", [])
if not isinstance(suite_entries, list):
    suite_entries = []
passing_suites = []
failing_suites = []
for suite in suite_entries:
    if not isinstance(suite, dict):
        continue
    suite_name = str(suite.get("suite", "")).strip()
    if not suite_name:
        continue
    if as_int(suite.get("exit_code"), default=1) == 0:
        passing_suites.append(suite_name)
    else:
        failing_suites.append(suite_name)

unit_entries = summary.get("unit_targets", [])
if not isinstance(unit_entries, list):
    unit_entries = []
failing_unit_targets = []
for target in unit_entries:
    if not isinstance(target, dict):
        continue
    target_name = str(target.get("target", "")).strip()
    if not target_name:
        continue
    if as_int(target.get("exit_code"), default=1) != 0:
        failing_unit_targets.append(target_name)

profiles_payload = profile_matrix.get("profiles", {}) if isinstance(profile_matrix, dict) else {}
if not isinstance(profiles_payload, dict):
    profiles_payload = {}

consistency = (
    profile_matrix.get("preflight_runtime_consistency", {})
    if isinstance(profile_matrix, dict)
    else {}
)
if not isinstance(consistency, dict):
    consistency = {}
mismatches = as_int(consistency.get("mismatched_capability_decisions"))
checked = as_int(consistency.get("checked_capability_decisions"))

profile_order = ["safe", "balanced", "permissive"]
profile_recommendation_defaults = {
    "safe": "Default profile for untrusted extensions",
    "balanced": "Recommended for general extension development",
    "permissive": "Use only for trusted local extensions",
}

profile_rows = {}
for profile_name in profile_order:
    row = profiles_payload.get(profile_name, {})
    if not isinstance(row, dict):
        row = {}
    projected = row.get("projected", {})
    if not isinstance(projected, dict):
        projected = {}
    blocked_summary = row.get("blocked_operation_summary", {})
    if not isinstance(blocked_summary, dict):
        blocked_summary = {}

    blocked = as_int(projected.get("blocked"))
    pass_rate_pct = as_float(projected.get("pass_rate_pct"))
    testable = as_int(projected.get("testable_extensions"))
    projected_pass = as_int(projected.get("pass"))
    projected_fail_existing = as_int(projected.get("fail_existing"))

    if profile_name == "balanced" and pass_rate_pct >= 85.0 and blocked <= 5:
        recommendation = "Recommended default for most users"
    elif profile_name == "safe":
        recommendation = "Safest default with strongest capability restrictions"
    elif profile_name == "permissive":
        recommendation = "Trusted-only mode; enables dangerous capabilities"
    else:
        recommendation = profile_recommendation_defaults[profile_name]

    profile_rows[profile_name] = {
        "projected_pass_rate_pct": round(pass_rate_pct, 2),
        "projected_pass": projected_pass,
        "projected_fail_existing": projected_fail_existing,
        "blocked_extensions": blocked,
        "testable_extensions": testable,
        "recommendation": recommendation,
        "blocked_operation_summary": blocked_summary,
    }

soak_pass = bool(soak_report.get("pass")) if isinstance(soak_report, dict) else False
soak_status = str(soak_report.get("status", "missing")) if isinstance(soak_report, dict) else "missing"
soak_failed_checks = (
    soak_report.get("failed_checks", []) if isinstance(soak_report, dict) else []
)
if not isinstance(soak_failed_checks, list):
    soak_failed_checks = []

overall_ready = (
    failed_units == 0
    and failed_suites == 0
    and conformance_fail == 0
    and conformance_na == 0
    and mismatches == 0
    and soak_pass
)

known_risks = []
remediation_steps = []


def add_risk(risk_id, severity, summary_text, details, evidence, command):
    known_risks.append(
        {
            "id": risk_id,
            "severity": severity,
            "summary": summary_text,
            "details": details,
            "evidence": evidence,
        }
    )
    if command:
        remediation_steps.append(
            {
                "risk_id": risk_id,
                "action": summary_text,
                "command": command,
            }
        )


if failed_units > 0:
    add_risk(
        "unit_target_failures",
        "high",
        "One or more integration/unit targets failed in the verification run",
        f"Failing unit targets: {failing_unit_targets}",
        str(summary_file),
        "./scripts/e2e/run_all.sh --profile focused --skip-lint",
    )

if failed_suites > 0:
    add_risk(
        "e2e_suite_failures",
        "high",
        "One or more user-facing workflow suites failed",
        f"Failing suites: {failing_suites}",
        str(summary_file),
        " ".join(
            [
                "./scripts/e2e/run_all.sh",
                "--profile",
                "focused",
                "--skip-lint",
                *sum((["--suite", suite] for suite in failing_suites), []),
            ]
        ),
    )

if conformance_fail > 0:
    add_risk(
        "conformance_failures",
        "high",
        "Conformance corpus has failing extensions",
        f"{conformance_fail} extension(s) currently fail conformance",
        str(conformance_summary_file),
        "cargo test --test ext_conformance_generated conformance_full_report --features ext-conformance -- --nocapture",
    )

if conformance_na > 0:
    add_risk(
        "conformance_na_gaps",
        "medium",
        "Conformance corpus still includes N/A coverage gaps",
        f"{conformance_na} extension(s) are not yet classified PASS/FAIL",
        str(conformance_summary_file),
        "cargo test --test ext_conformance_scenarios --features ext-conformance scenario_conformance_suite -- --nocapture",
    )

if mismatches > 0:
    add_risk(
        "profile_policy_mismatch",
        "high",
        "Preflight/runtime capability policy mismatch detected",
        f"{mismatches} mismatched capability decision(s) out of {checked} checked",
        str(profile_matrix_file),
        "cargo test --test e2e_cli -- --nocapture",
    )

if not soak_pass:
    add_risk(
        "soak_longevity_failure",
        "high",
        "Soak/longevity verification failed thresholds",
        f"status={soak_status}, failed_checks={soak_failed_checks}",
        str(soak_file),
        "cargo test --test ext_memory_stress ext_memory_stress_inline -- --nocapture && cargo test --test extensions_stress stress_policy_profile_rotation -- --nocapture",
    )

if conformance_report_md.exists():
    report_has_evidence_index = "## Evidence Index" in conformance_report_md.read_text(
        encoding="utf-8"
    )
    if not report_has_evidence_index:
        add_risk(
            "missing_evidence_index",
            "medium",
            "Conformance markdown report missing evidence index section",
            "Expected '## Evidence Index' section not found",
            str(conformance_report_md),
            "cargo test --test conformance_report generate_conformance_report -- --nocapture",
        )

policy_guidance = []
for profile_name in profile_order:
    policy_guidance.append(
        {
            "profile": profile_name,
            "guidance": profile_recommendation_defaults[profile_name],
            "diagnostic_command": (
                f"./target/debug/pi --explain-extension-policy --extension-policy {profile_name}"
            ),
        }
    )

workflow_pass_rate = 100.0
if total_suites > 0:
    workflow_pass_rate = round(((total_suites - failed_suites) / total_suites) * 100.0, 2)

status = "ready" if overall_ready else "not_ready"

payload = {
    "schema": "pi.e2e.release_readiness.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "status": status,
    "overall_ready": overall_ready,
    "summary": {
        "profile": str(summary.get("profile", "unknown")),
        "conformance_total": conformance_total,
        "conformance_pass": conformance_pass,
        "conformance_fail": conformance_fail,
        "conformance_na": conformance_na,
        "conformance_pass_rate_pct": round(conformance_pass_rate, 2),
        "total_units": total_units,
        "failed_units": failed_units,
        "total_suites": total_suites,
        "failed_suites": failed_suites,
    },
    "profiles": profile_rows,
    "workflow_outcomes": {
        "passing_suites": sorted(passing_suites),
        "failing_suites": sorted(failing_suites),
        "failing_unit_targets": sorted(failing_unit_targets),
        "suite_pass_rate_pct": workflow_pass_rate,
    },
    "known_risks": known_risks,
    "remediation": {
        "immediate": remediation_steps,
        "policy_guidance": policy_guidance,
    },
    "evidence": {
        "summary_json": str(summary_file),
        "profile_matrix_json": str(profile_matrix_file),
        "soak_longevity_report_json": str(soak_file),
        "conformance_summary_json": str(conformance_summary_file),
        "conformance_report_markdown": str(conformance_report_md),
        "evidence_contract_json": str(contract_file),
    },
}

readiness_file.parent.mkdir(parents=True, exist_ok=True)
readiness_file.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

lines = [
    "# Release Readiness Summary",
    "",
    f"- Generated at: `{payload['generated_at']}`",
    f"- Status: `{status}`",
    f"- Overall ready: `{overall_ready}`",
    "",
    "## Profile Safety Snapshot",
    "",
    "| Profile | Projected pass rate | Blocked extensions | Recommendation |",
    "|---|---:|---:|---|",
]
for profile_name in profile_order:
    row = profile_rows.get(profile_name, {})
    lines.append(
        f"| {profile_name} | {row.get('projected_pass_rate_pct', 0.0):.2f}% | "
        f"{row.get('blocked_extensions', 0)} | {row.get('recommendation', '')} |"
    )

lines.extend(
    [
        "",
        "## Workflow Outcomes",
        "",
        f"- Suite pass rate: `{workflow_pass_rate:.2f}%`",
        f"- Passing suites ({len(passing_suites)}): "
        + (", ".join(sorted(passing_suites)) if passing_suites else "none"),
        f"- Failing suites ({len(failing_suites)}): "
        + (", ".join(sorted(failing_suites)) if failing_suites else "none"),
        f"- Failing unit targets ({len(failing_unit_targets)}): "
        + (", ".join(sorted(failing_unit_targets)) if failing_unit_targets else "none"),
        "",
        "## Known Risks",
        "",
    ]
)

if known_risks:
    for risk in known_risks:
        lines.extend(
            [
                f"- `{risk['id']}` ({risk['severity']}): {risk['summary']}",
                f"  - details: {risk['details']}",
                f"  - evidence: `{risk['evidence']}`",
            ]
        )
else:
    lines.append("- None")

lines.extend(
    [
        "",
        "## Recommended Remediation",
        "",
    ]
)

if remediation_steps:
    for step in remediation_steps:
        lines.extend(
            [
                f"- `{step['risk_id']}`: {step['action']}",
                f"  - command: `{step['command']}`",
            ]
        )
else:
    lines.append("- No immediate remediation required.")

lines.extend(
    [
        "",
        "## Policy Guidance",
        "",
    ]
)
for guidance in policy_guidance:
    lines.extend(
        [
            f"- `{guidance['profile']}`: {guidance['guidance']}",
            f"  - diagnostic: `{guidance['diagnostic_command']}`",
        ]
    )

readiness_markdown.write_text("\n".join(lines) + "\n", encoding="utf-8")

if isinstance(summary, dict):
    summary["release_readiness"] = {
        "schema": "pi.e2e.release_readiness.v1",
        "path": str(readiness_file),
        "markdown_path": str(readiness_markdown),
        "status": status,
        "overall_ready": overall_ready,
        "risk_count": len(known_risks),
        "failing_suites": sorted(failing_suites),
        "failing_unit_targets": sorted(failing_unit_targets),
    }
    summary_file.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

print("RELEASE READINESS SUMMARY GENERATED")
print(f"- JSON: {readiness_file}")
print(f"- Markdown: {readiness_markdown}")
print(f"- Status: {status}")
print(f"- Risks: {len(known_risks)}")
PY
    then
        echo "[readiness] Release-readiness summary generated ($readiness_file)"
        return 0
    else
        echo "[readiness] Failed to generate release-readiness summary" >&2
        return 1
    fi
}

# ─── Secret Redaction ─────────────────────────────────────────────────────────

redact_secrets() {
    # Redact common API key patterns from all log files.
    local patterns=(
        's/sk-[a-zA-Z0-9_-]{20,}/sk-REDACTED/g'
        's/key-[a-zA-Z0-9_-]{20,}/key-REDACTED/g'
        's/ANTHROPIC_API_KEY=[^ ]*/ANTHROPIC_API_KEY=REDACTED/g'
        's/OPENAI_API_KEY=[^ ]*/OPENAI_API_KEY=REDACTED/g'
        's/GOOGLE_API_KEY=[^ ]*/GOOGLE_API_KEY=REDACTED/g'
        's/AZURE_OPENAI_API_KEY=[^ ]*/AZURE_OPENAI_API_KEY=REDACTED/g'
    )

    local sed_args=()
    for pattern in "${patterns[@]}"; do
        sed_args+=(-e "$pattern")
    done

    # Find all log/jsonl files and redact in-place.
    find "$ARTIFACT_DIR" -type f \( -name "*.log" -o -name "*.jsonl" \) -print0 | \
        xargs -0 -r sed -i "${sed_args[@]}" 2>/dev/null || true
}

# ─── Evidence Diff + Triage ──────────────────────────────────────────────────

generate_triage_diff() {
    local baseline_summary="$DIFF_FROM"
    local current_summary="$ARTIFACT_DIR/summary.json"
    local diff_file="$ARTIFACT_DIR/triage_diff.json"

    if [[ -z "$baseline_summary" ]]; then
        return 0
    fi
    if [[ ! -f "$baseline_summary" ]]; then
        echo "[triage] Baseline summary not found: $baseline_summary" >&2
        return 1
    fi
    if [[ ! -f "$current_summary" ]]; then
        echo "[triage] Current summary missing: $current_summary" >&2
        return 1
    fi

    if CURRENT_SUMMARY="$current_summary" \
        BASELINE_SUMMARY="$baseline_summary" \
        DIFF_FILE="$diff_file" \
        python3 - <<'PY'
import json
import os
import shlex
import sys
from datetime import datetime, timezone
from pathlib import Path

current_summary_path = Path(os.environ["CURRENT_SUMMARY"])
baseline_summary_path = Path(os.environ["BASELINE_SUMMARY"])
diff_file = Path(os.environ["DIFF_FILE"])


def read_summary(path: Path) -> dict:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        raise RuntimeError(f"failed to read summary {path}: {exc}") from exc
    if not isinstance(payload, dict):
        raise RuntimeError(f"summary {path} is not a JSON object")
    return payload


def build_index(summary: dict, section_key: str, name_key: str) -> dict[str, dict]:
    indexed: dict[str, dict] = {}
    section = summary.get(section_key, [])
    if not isinstance(section, list):
        return indexed
    for item in section:
        if not isinstance(item, dict):
            continue
        name = item.get(name_key)
        if isinstance(name, str) and name.strip():
            indexed[name] = item
    return indexed


def classify_change(
    *,
    name: str,
    baseline: dict | None,
    current: dict | None,
) -> tuple[str, dict]:
    baseline_exit = None if baseline is None else baseline.get("exit_code")
    current_exit = None if current is None else current.get("exit_code")

    baseline_ok = baseline_exit == 0
    current_ok = current_exit == 0

    record = {
        "name": name,
        "baseline_exit_code": baseline_exit,
        "current_exit_code": current_exit,
        "baseline_log_file": None if baseline is None else baseline.get("log_file"),
        "current_log_file": None if current is None else current.get("log_file"),
    }

    if baseline is None and current is not None:
        return ("added_pass" if current_ok else "new_failure"), record
    if baseline is not None and current is None:
        return "removed", record
    if baseline_ok and not current_ok:
        return "regression", record
    if not baseline_ok and current_ok:
        return "fixed", record
    if not baseline_ok and not current_ok:
        return "unresolved_failure", record
    return "stable_pass", record


def diff_section(
    *,
    baseline: dict,
    current: dict,
    section_key: str,
    name_key: str,
) -> dict:
    baseline_index = build_index(baseline, section_key, name_key)
    current_index = build_index(current, section_key, name_key)

    categories = {
        "regressions": [],
        "fixed": [],
        "unresolved_failures": [],
        "new_failures": [],
        "added_pass": [],
        "removed": [],
        "stable_pass": [],
    }

    all_names = sorted(set(baseline_index) | set(current_index))
    for name in all_names:
        category, record = classify_change(
            name=name,
            baseline=baseline_index.get(name),
            current=current_index.get(name),
        )
        if category == "new_failure":
            categories["new_failures"].append(record)
        else:
            categories[category if category in categories else "stable_pass"].append(record)

    return {
        "baseline_total": len(baseline_index),
        "current_total": len(current_index),
        **categories,
    }


def build_runner_command(unit_targets: list[str], suites: list[str]) -> str:
    parts = ["./scripts/e2e/run_all.sh", "--profile", "focused", "--skip-lint"]
    if unit_targets:
        for target in unit_targets:
            parts.extend(["--unit-target", target])
    else:
        parts.append("--skip-unit")
    for suite in suites:
        parts.extend(["--suite", suite])
    return " ".join(shlex.quote(part) for part in parts)


def collect_names(records: list[dict]) -> list[str]:
    return [str(record["name"]) for record in records if isinstance(record.get("name"), str)]


def build_target_commands(unit_names: list[str], suite_names: list[str]) -> dict:
    commands: dict[str, str] = {}
    for name in unit_names:
        commands[f"unit:{name}"] = f"cargo test --test {shlex.quote(name)} -- --nocapture"
    for name in suite_names:
        commands[f"suite:{name}"] = f"cargo test --test {shlex.quote(name)} -- --nocapture"
    return commands


def build_ranked_diagnostics(unit_diff: dict, suite_diff: dict) -> list[dict]:
    category_priority = {
        "regressions": 100,
        "new_failures": 90,
        "unresolved_failures": 70,
        "fixed": 40,
        "removed": 30,
        "added_pass": 20,
        "stable_pass": 10,
    }
    category_label = {
        "regressions": "regression",
        "new_failures": "new_failure",
        "unresolved_failures": "unresolved_failure",
        "fixed": "fixed",
        "removed": "removed",
        "added_pass": "added_pass",
        "stable_pass": "stable_pass",
    }
    category_order = [
        "regressions",
        "new_failures",
        "unresolved_failures",
        "fixed",
        "removed",
        "added_pass",
        "stable_pass",
    ]

    diagnostics: list[dict] = []
    for surface, section in (("unit", unit_diff), ("suite", suite_diff)):
        for category in category_order:
            for record in section.get(category, []):
                name = record.get("name")
                if not isinstance(name, str) or not name:
                    continue
                diagnostics.append(
                    {
                        "surface": surface,
                        "name": name,
                        "severity": category_label[category],
                        "priority_score": category_priority[category],
                        "baseline_exit_code": record.get("baseline_exit_code"),
                        "current_exit_code": record.get("current_exit_code"),
                        "baseline_log_file": record.get("baseline_log_file"),
                        "current_log_file": record.get("current_log_file"),
                        "recommended_command": (
                            f"cargo test --test {shlex.quote(name)} -- --nocapture"
                        ),
                    }
                )

    diagnostics.sort(
        key=lambda item: (
            -int(item["priority_score"]),
            str(item["surface"]),
            str(item["name"]),
        )
    )
    for index, item in enumerate(diagnostics, start=1):
        item["rank"] = index

    return diagnostics


try:
    baseline_summary = read_summary(baseline_summary_path)
    current_summary = read_summary(current_summary_path)
except RuntimeError as exc:
    print(f"[triage] {exc}", file=sys.stderr)
    sys.exit(1)

unit_diff = diff_section(
    baseline=baseline_summary,
    current=current_summary,
    section_key="unit_targets",
    name_key="target",
)
suite_diff = diff_section(
    baseline=baseline_summary,
    current=current_summary,
    section_key="suites",
    name_key="suite",
)

unit_regressions = collect_names(unit_diff["regressions"])
unit_new_failures = collect_names(unit_diff["new_failures"])
unit_unresolved = collect_names(unit_diff["unresolved_failures"])
suite_regressions = collect_names(suite_diff["regressions"])
suite_new_failures = collect_names(suite_diff["new_failures"])
suite_unresolved = collect_names(suite_diff["unresolved_failures"])

triage_unit_focus = sorted(set(unit_regressions + unit_new_failures + unit_unresolved))
triage_suite_focus = sorted(set(suite_regressions + suite_new_failures + suite_unresolved))

regression_count = len(unit_regressions) + len(suite_regressions)
new_failure_count = len(unit_new_failures) + len(suite_new_failures)
unresolved_count = len(unit_unresolved) + len(suite_unresolved)
fixed_count = len(unit_diff["fixed"]) + len(suite_diff["fixed"])

recommended_commands = {
    "runner_repro_command": build_runner_command(triage_unit_focus, triage_suite_focus)
    if triage_unit_focus or triage_suite_focus
    else "",
    "target_commands": build_target_commands(triage_unit_focus, triage_suite_focus),
}
ranked_diagnostics = build_ranked_diagnostics(unit_diff, suite_diff)
ranked_repro_commands: list[str] = []
seen_commands: set[str] = set()
for item in ranked_diagnostics:
    severity = item.get("severity")
    command = item.get("recommended_command")
    if severity not in {"regression", "new_failure", "unresolved_failure"}:
        continue
    if not isinstance(command, str) or not command:
        continue
    if command in seen_commands:
        continue
    seen_commands.add(command)
    ranked_repro_commands.append(command)
recommended_commands["ranked_repro_commands"] = ranked_repro_commands

status = "regression" if (regression_count > 0 or new_failure_count > 0) else "stable"
if status == "stable" and unresolved_count > 0:
    status = "known_failures_only"

payload = {
    "schema": "pi.e2e.triage_diff.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "status": status,
    "baseline_summary_path": str(baseline_summary_path),
    "current_summary_path": str(current_summary_path),
    "baseline_artifact_dir": baseline_summary.get("artifact_dir"),
    "current_artifact_dir": current_summary.get("artifact_dir"),
    "summary": {
        "regression_count": regression_count,
        "new_failure_count": new_failure_count,
        "unresolved_failure_count": unresolved_count,
        "fixed_count": fixed_count,
        "unit_regression_count": len(unit_regressions),
        "suite_regression_count": len(suite_regressions),
    },
    "unit_targets": unit_diff,
    "suites": suite_diff,
    "focus": {
        "unit_targets": triage_unit_focus,
        "suites": triage_suite_focus,
    },
    "ranked_diagnostics": ranked_diagnostics,
    "recommended_commands": recommended_commands,
}

diff_file.parent.mkdir(parents=True, exist_ok=True)
diff_file.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

current_summary["triage_diff"] = {
    "schema": "pi.e2e.triage_diff.v1",
    "path": str(diff_file),
    "baseline_summary_path": str(baseline_summary_path),
    "status": status,
    "regression_count": regression_count,
    "new_failure_count": new_failure_count,
    "unresolved_failure_count": unresolved_count,
    "fixed_count": fixed_count,
    "top_ranked_diagnostics": ranked_diagnostics[:5],
}
current_summary_path.write_text(json.dumps(current_summary, indent=2) + "\n", encoding="utf-8")

print("TRIAGE DIFF GENERATED")
print(f"- Baseline: {baseline_summary_path}")
print(f"- Current:  {current_summary_path}")
print(
    "- Summary: regressions="
    f"{regression_count}, new_failures={new_failure_count}, unresolved={unresolved_count}, fixed={fixed_count}"
)
if recommended_commands["runner_repro_command"]:
    print(f"- Repro runner: {recommended_commands['runner_repro_command']}")
if ranked_diagnostics:
    print("- Ranked diagnostics:")
    for item in ranked_diagnostics[:5]:
        print(
            f"  {item['rank']}. [{item['severity']}] "
            f"{item['surface']}:{item['name']} -> {item['recommended_command']}"
        )
print(f"- Diff artifact: {diff_file}")
sys.exit(0)
PY
    then
        echo "[triage] Diff report written to $diff_file"
        return 0
    else
        echo "[triage] Diff generation failed" >&2
        return 1
    fi
}

# ─── Evidence Contract Validation ────────────────────────────────────────────

validate_evidence_contract() {
    local contract_file="$ARTIFACT_DIR/evidence_contract.json"
    local selected_units_json selected_suites_json
    local all_unit_targets_json all_suites_json

    selected_units_json="$(
        printf '%s\n' "${SELECTED_UNIT_TARGETS[@]:-}" | \
            python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'
    )"
    selected_suites_json="$(
        printf '%s\n' "${SELECTED_SUITES[@]:-}" | \
            python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'
    )"
    all_unit_targets_json="$(
        printf '%s\n' "${ALL_UNIT_TARGETS[@]:-}" | \
            python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'
    )"
    all_suites_json="$(
        printf '%s\n' "${ALL_SUITES[@]:-}" | \
            python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'
    )"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        PROJECT_ROOT="$PROJECT_ROOT" \
        VERIFY_PROFILE_NAME="$PROFILE" \
        CONTRACT_FILE="$contract_file" \
        SELECTED_UNITS_JSON="$selected_units_json" \
        SELECTED_SUITES_JSON="$selected_suites_json" \
        ALL_UNIT_TARGETS_JSON="$all_unit_targets_json" \
        ALL_SUITES_JSON="$all_suites_json" \
        RERUN_FROM_JSON="$RERUN_JSON_VALUE" \
        python3 - <<'PY'
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
profile = os.environ.get("VERIFY_PROFILE_NAME", "full")
contract_file = Path(os.environ["CONTRACT_FILE"])

try:
    selected_units = json.loads(os.environ.get("SELECTED_UNITS_JSON", "[]"))
except json.JSONDecodeError:
    selected_units = []
try:
    selected_suites = json.loads(os.environ.get("SELECTED_SUITES_JSON", "[]"))
except json.JSONDecodeError:
    selected_suites = []
try:
    all_unit_targets = json.loads(os.environ.get("ALL_UNIT_TARGETS_JSON", "[]"))
except json.JSONDecodeError:
    all_unit_targets = []
try:
    all_suites = json.loads(os.environ.get("ALL_SUITES_JSON", "[]"))
except json.JSONDecodeError:
    all_suites = []
try:
    rerun_from = json.loads(os.environ.get("RERUN_FROM_JSON", "null"))
except json.JSONDecodeError:
    rerun_from = None

checks = []
errors = []
warnings = []


def add_check(check_id: str, path: Path, ok: bool, diagnostics: str) -> None:
    checks.append(
        {
            "id": check_id,
            "path": str(path),
            "ok": ok,
            "diagnostics": diagnostics,
        }
    )


def require_file(check_id: str, path: Path, *, strict: bool, description: str) -> bool:
    ok = path.exists()
    add_check(check_id, path, ok, description if ok else f"missing required file: {path}")
    if not ok:
        if strict:
            errors.append(f"{check_id}: missing {path}")
        else:
            warnings.append(f"{check_id}: missing {path}")
    return ok


def load_json(check_id: str, path: Path, *, strict: bool) -> dict | None:
    if not require_file(check_id, path, strict=strict, description="file exists"):
        return None
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:  # pragma: no cover - defensive
        add_check(check_id + ".json_parse", path, False, f"invalid JSON: {exc}")
        if strict:
            errors.append(f"{check_id}: invalid JSON ({exc})")
        else:
            warnings.append(f"{check_id}: invalid JSON ({exc})")
        return None
    add_check(check_id + ".json_parse", path, True, "valid JSON")
    return payload


def require_keys(
    check_id: str,
    payload: dict | None,
    path: Path,
    keys: list[str],
    *,
    strict: bool,
) -> None:
    if payload is None:
        return
    missing = [key for key in keys if key not in payload]
    ok = not missing
    add_check(
        check_id + ".keys",
        path,
        ok,
        "all required keys present" if ok else f"missing keys: {', '.join(missing)}",
    )
    if not ok:
        msg = f"{check_id}: missing keys in {path}: {', '.join(missing)}"
        if strict:
            errors.append(msg)
        else:
            warnings.append(msg)


def require_condition(
    check_id: str,
    *,
    path: Path,
    ok: bool,
    ok_msg: str,
    fail_msg: str,
    strict: bool,
) -> None:
    add_check(check_id, path, ok, ok_msg if ok else fail_msg)
    if ok:
        return
    if strict:
        errors.append(f"{check_id}: {fail_msg}")
    else:
        warnings.append(f"{check_id}: {fail_msg}")


# 1) Core run artifacts (always required)
environment_path = artifact_dir / "environment.json"
environment = load_json("environment", environment_path, strict=True)
require_keys(
    "environment",
    environment,
    environment_path,
    [
        "timestamp",
        "profile",
        "rustc",
        "cargo",
        "os",
        "git_sha",
        "git_branch",
        "parallelism",
        "log_level",
        "artifact_dir",
    ],
    strict=True,
)

summary_path = artifact_dir / "summary.json"
summary = load_json("summary", summary_path, strict=True)
require_keys(
    "summary",
    summary,
    summary_path,
    [
        "timestamp",
        "profile",
        "artifact_dir",
        "total_units",
        "passed_units",
        "failed_units",
        "total_suites",
        "passed_suites",
        "failed_suites",
        "unit_targets",
        "suites",
    ],
    strict=True,
)

# Full-profile baseline runs must cover every configured target and suite.
strict_conformance = profile == "full" and rerun_from is None
full_scope_path = artifact_dir / "summary.json"
if profile == "full":
    require_condition(
        "full_profile.rerun_mode",
        path=full_scope_path,
        ok=rerun_from is None,
        ok_msg="full profile baseline run (not rerun)",
        fail_msg="full profile rerun detected; scope checks downgraded to warnings",
        strict=False,
    )

    missing_units = sorted(set(all_unit_targets) - set(selected_units))
    extra_units = sorted(set(selected_units) - set(all_unit_targets))
    require_condition(
        "full_profile.unit_scope_complete",
        path=full_scope_path,
        ok=not missing_units and not extra_units,
        ok_msg=f"selected all unit targets ({len(all_unit_targets)})",
        fail_msg=(
            f"unit scope mismatch; missing={missing_units if missing_units else []}, "
            f"extra={extra_units if extra_units else []}"
        ),
        strict=strict_conformance,
    )

    missing_suites = sorted(set(all_suites) - set(selected_suites))
    extra_suites = sorted(set(selected_suites) - set(all_suites))
    require_condition(
        "full_profile.e2e_scope_complete",
        path=full_scope_path,
        ok=not missing_suites and not extra_suites,
        ok_msg=f"selected all e2e suites ({len(all_suites)})",
        fail_msg=(
            f"e2e scope mismatch; missing={missing_suites if missing_suites else []}, "
            f"extra={extra_suites if extra_suites else []}"
        ),
        strict=strict_conformance,
    )

for target in selected_units:
    result_path = artifact_dir / "unit" / target / "result.json"
    result = load_json(f"unit:{target}:result", result_path, strict=True)
    require_keys(
        f"unit:{target}:result",
        result,
        result_path,
        ["target", "exit_code", "duration_ms", "passed", "failed", "ignored", "total", "log_file"],
        strict=True,
    )
    if isinstance(result, dict):
        log_file = Path(str(result.get("log_file", "")))
        ok = log_file.exists()
        add_check(
            f"unit:{target}:log_file",
            log_file,
            ok,
            "referenced log file exists" if ok else f"result.json references missing log: {log_file}",
        )
        if not ok:
            errors.append(f"unit:{target}: missing log file {log_file}")

for suite in selected_suites:
    result_path = artifact_dir / suite / "result.json"
    result = load_json(f"suite:{suite}:result", result_path, strict=True)
    require_keys(
        f"suite:{suite}:result",
        result,
        result_path,
        ["suite", "exit_code", "duration_ms", "passed", "failed", "ignored", "total", "log_file"],
        strict=True,
    )
    if isinstance(result, dict):
        log_file = Path(str(result.get("log_file", "")))
        ok = log_file.exists()
        add_check(
            f"suite:{suite}:log_file",
            log_file,
            ok,
            "referenced log file exists" if ok else f"result.json references missing log: {log_file}",
        )
        if not ok:
            errors.append(f"suite:{suite}: missing log file {log_file}")


# 2) Conformance evidence artifacts (strict for full baseline profile)
reports_dir = project_root / "tests" / "ext_conformance" / "reports"
events_path = reports_dir / "conformance_events.jsonl"
summary_report_path = reports_dir / "conformance_summary.json"
markdown_path = reports_dir / "CONFORMANCE_REPORT.md"

events_exists = require_file(
    "conformance.events_jsonl",
    events_path,
    strict=strict_conformance,
    description="per-extension JSONL evidence log exists",
)
summary_report = load_json(
    "conformance.summary_json",
    summary_report_path,
    strict=strict_conformance,
)
require_keys(
    "conformance.summary_json",
    summary_report,
    summary_report_path,
    ["schema", "generated_at", "counts", "pass_rate_pct", "evidence"],
    strict=strict_conformance,
)

if isinstance(summary_report, dict):
    evidence_payload = summary_report.get("evidence")
    require_condition(
        "conformance.summary_json.evidence_object",
        path=summary_report_path,
        ok=isinstance(evidence_payload, dict),
        ok_msg="evidence payload is an object",
        fail_msg="missing or invalid evidence object",
        strict=strict_conformance,
    )
    if isinstance(evidence_payload, dict):
        missing_evidence_keys = sorted(
            set(["golden_fixtures", "load_time_benchmarks", "parity_logs", "smoke_logs"]) -
            set(evidence_payload.keys())
        )
        require_condition(
            "conformance.summary_json.evidence_keys",
            path=summary_report_path,
            ok=not missing_evidence_keys,
            ok_msg="evidence payload includes required keys",
            fail_msg=f"missing evidence keys: {missing_evidence_keys}",
            strict=strict_conformance,
        )

    schema = summary_report.get("schema")
    schema_ok = schema == "pi.ext.conformance_summary.v2"
    add_check(
        "conformance.summary_schema",
        summary_report_path,
        schema_ok,
        f"schema={schema!r}" if schema_ok else f"unexpected schema {schema!r}",
    )
    if not schema_ok:
        message = (
            f"conformance.summary_schema: expected 'pi.ext.conformance_summary.v2', got {schema!r}"
        )
        if strict_conformance:
            errors.append(message)
        else:
            warnings.append(message)

markdown_exists = require_file(
    "conformance.report_markdown",
    markdown_path,
    strict=strict_conformance,
    description="human-readable conformance report exists",
)
if markdown_exists:
    content = markdown_path.read_text(encoding="utf-8")
    has_index = "## Evidence Index" in content
    add_check(
        "conformance.report_markdown.evidence_index",
        markdown_path,
        has_index,
        "contains Evidence Index section" if has_index else "missing '## Evidence Index'",
    )
    if not has_index:
        message = f"conformance.report_markdown: missing Evidence Index in {markdown_path}"
        if strict_conformance:
            errors.append(message)
        else:
            warnings.append(message)

if events_exists:
    try:
        lines = [line for line in events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    except Exception as exc:  # pragma: no cover - defensive
        add_check("conformance.events_jsonl.readable", events_path, False, f"read failed: {exc}")
        if strict_conformance:
            errors.append(f"conformance.events_jsonl: read failed ({exc})")
        else:
            warnings.append(f"conformance.events_jsonl: read failed ({exc})")
        lines = []
    if lines:
        add_check("conformance.events_jsonl.non_empty", events_path, True, f"{len(lines)} line(s)")
        first_invalid = None
        for index, line in enumerate(lines, start=1):
            try:
                json.loads(line)
            except Exception as exc:  # pragma: no cover - defensive
                first_invalid = f"line {index}: {exc}"
                break
        require_condition(
            "conformance.events_jsonl.valid_json",
            path=events_path,
            ok=first_invalid is None,
            ok_msg="every JSONL entry parsed successfully",
            fail_msg=f"invalid JSONL entry ({first_invalid})",
            strict=strict_conformance,
        )
    else:
        add_check("conformance.events_jsonl.non_empty", events_path, False, "no JSONL entries found")
        message = f"conformance.events_jsonl: empty file {events_path}"
        if strict_conformance:
            errors.append(message)
        else:
            warnings.append(message)

# 3) Capability-profile matrix evidence (always required)
profile_matrix_path = artifact_dir / "extension_profile_matrix.json"
profile_matrix = load_json(
    "conformance.profile_matrix_json",
    profile_matrix_path,
    strict=True,
)
require_keys(
    "conformance.profile_matrix_json",
    profile_matrix,
    profile_matrix_path,
    ["schema", "generated_at", "profiles", "preflight_runtime_consistency"],
    strict=True,
)

if isinstance(profile_matrix, dict):
    schema = profile_matrix.get("schema")
    require_condition(
        "conformance.profile_matrix_schema",
        path=profile_matrix_path,
        ok=schema == "pi.e2e.extension_profile_matrix.v1",
        ok_msg="profile matrix schema matches",
        fail_msg=(
            "expected schema 'pi.e2e.extension_profile_matrix.v1', "
            f"got {schema!r}"
        ),
        strict=True,
    )

    profiles_payload = profile_matrix.get("profiles")
    profiles_ok = isinstance(profiles_payload, dict)
    require_condition(
        "conformance.profile_matrix_profiles_object",
        path=profile_matrix_path,
        ok=profiles_ok,
        ok_msg="profiles object exists",
        fail_msg="profiles must be an object",
        strict=True,
    )

    if profiles_ok:
        required_profiles = ["safe", "balanced", "permissive"]
        missing_profiles = [
            profile_name
            for profile_name in required_profiles
            if profile_name not in profiles_payload
        ]
        require_condition(
            "conformance.profile_matrix_profiles_complete",
            path=profile_matrix_path,
            ok=not missing_profiles,
            ok_msg="safe|balanced|permissive profile entries exist",
            fail_msg=f"missing profile entries: {missing_profiles}",
            strict=True,
        )

        for profile_name in required_profiles:
            profile_entry = profiles_payload.get(profile_name)
            prefix = f"conformance.profile_matrix.{profile_name}"
            require_condition(
                f"{prefix}.object",
                path=profile_matrix_path,
                ok=isinstance(profile_entry, dict),
                ok_msg=f"{profile_name} profile entry is object",
                fail_msg=f"{profile_name} profile entry must be object",
                strict=True,
            )
            if not isinstance(profile_entry, dict):
                continue

            projected = profile_entry.get("projected")
            require_condition(
                f"{prefix}.projected_object",
                path=profile_matrix_path,
                ok=isinstance(projected, dict),
                ok_msg="projected object exists",
                fail_msg="projected must be an object",
                strict=True,
            )

            blocked_summary = profile_entry.get("blocked_operation_summary")
            require_condition(
                f"{prefix}.blocked_summary",
                path=profile_matrix_path,
                ok=isinstance(blocked_summary, dict),
                ok_msg="blocked_operation_summary object exists",
                fail_msg="blocked_operation_summary must be an object",
                strict=True,
            )

            if isinstance(projected, dict):
                for field in (
                    "testable_extensions",
                    "pass",
                    "fail_existing",
                    "blocked",
                    "pass_rate_pct",
                ):
                    require_condition(
                        f"{prefix}.projected.{field}",
                        path=profile_matrix_path,
                        ok=field in projected,
                        ok_msg=f"projected.{field} present",
                        fail_msg=f"missing projected.{field}",
                        strict=True,
                    )

    consistency = profile_matrix.get("preflight_runtime_consistency")
    require_condition(
        "conformance.profile_matrix_consistency_object",
        path=profile_matrix_path,
        ok=isinstance(consistency, dict),
        ok_msg="preflight_runtime_consistency object exists",
        fail_msg="preflight_runtime_consistency must be an object",
        strict=True,
    )
    if isinstance(consistency, dict):
        checked = consistency.get("checked_capability_decisions")
        mismatched = consistency.get("mismatched_capability_decisions")
        require_condition(
            "conformance.profile_matrix_consistency_checked_nonzero",
            path=profile_matrix_path,
            ok=isinstance(checked, int) and checked > 0,
            ok_msg=f"checked_capability_decisions={checked}",
            fail_msg=(
                "checked_capability_decisions must be a positive integer "
                f"(got {checked!r})"
            ),
            strict=True,
        )
        require_condition(
            "conformance.profile_matrix_consistency_no_mismatch",
            path=profile_matrix_path,
            ok=isinstance(mismatched, int) and mismatched == 0,
            ok_msg="preflight/runtime consistency mismatches == 0",
            fail_msg=(
                "preflight/runtime consistency mismatches must be 0 "
                f"(got {mismatched!r})"
            ),
            strict=True,
        )


# 4) Soak/longevity evidence (bd-k5q5.7.10)
soak_report_path = artifact_dir / "soak_longevity_report.json"
soak_report = load_json(
    "conformance.soak_longevity_report_json",
    soak_report_path,
    strict=True,
)
require_keys(
    "conformance.soak_longevity_report_json",
    soak_report,
    soak_report_path,
    [
        "schema",
        "generated_at",
        "status",
        "pass",
        "prerequisites",
        "metrics",
        "thresholds",
        "checks",
        "failed_checks",
    ],
    strict=True,
)

if isinstance(soak_report, dict):
    soak_schema = soak_report.get("schema")
    require_condition(
        "conformance.soak_longevity_report_schema",
        path=soak_report_path,
        ok=soak_schema == "pi.e2e.soak_longevity.v1",
        ok_msg="soak/longevity schema matches",
        fail_msg=(
            "expected schema 'pi.e2e.soak_longevity.v1', "
            f"got {soak_schema!r}"
        ),
        strict=True,
    )

    prerequisites = soak_report.get("prerequisites")
    require_condition(
        "conformance.soak_longevity_prerequisites_object",
        path=soak_report_path,
        ok=isinstance(prerequisites, dict),
        ok_msg="prerequisites object exists",
        fail_msg="prerequisites must be an object",
        strict=True,
    )

    checks_payload = soak_report.get("checks")
    require_condition(
        "conformance.soak_longevity_checks_array",
        path=soak_report_path,
        ok=isinstance(checks_payload, list) and len(checks_payload) > 0,
        ok_msg=f"checks list contains {len(checks_payload) if isinstance(checks_payload, list) else 0} entries",
        fail_msg="checks must be a non-empty array",
        strict=True,
    )

    if strict_conformance and isinstance(prerequisites, dict):
        required_targets_executed = prerequisites.get("required_targets_executed")
        require_condition(
            "conformance.soak_longevity_required_targets_executed",
            path=soak_report_path,
            ok=required_targets_executed is True,
            ok_msg="required stress targets executed in this run",
            fail_msg=(
                "required stress targets were not executed in this run "
                "(expected ext_memory_stress + extensions_stress to pass)"
            ),
            strict=True,
        )

        report_pass = soak_report.get("pass")
        require_condition(
            "conformance.soak_longevity_pass",
            path=soak_report_path,
            ok=report_pass is True,
            ok_msg="soak/longevity verdict passed",
            fail_msg=f"soak/longevity verdict must pass for full baseline run (got {report_pass!r})",
            strict=True,
        )

soak_events_path = artifact_dir / "soak_longevity_events.jsonl"
soak_events_exists = require_file(
    "conformance.soak_longevity_events_jsonl",
    soak_events_path,
    strict=True,
    description="soak/longevity structured events log exists",
)
if soak_events_exists:
    lines = [line for line in soak_events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    require_condition(
        "conformance.soak_longevity_events_non_empty",
        path=soak_events_path,
        ok=len(lines) > 0,
        ok_msg=f"soak/longevity events contains {len(lines)} entries",
        fail_msg="soak/longevity events log is empty",
        strict=True,
    )


# 5) User-focused release-readiness summary (bd-k5q5.7.11)
release_readiness_path = artifact_dir / "release_readiness_summary.json"
release_readiness = load_json(
    "conformance.release_readiness_json",
    release_readiness_path,
    strict=True,
)
require_keys(
    "conformance.release_readiness_json",
    release_readiness,
    release_readiness_path,
    [
        "schema",
        "generated_at",
        "status",
        "overall_ready",
        "summary",
        "profiles",
        "workflow_outcomes",
        "known_risks",
        "remediation",
        "evidence",
    ],
    strict=True,
)

if isinstance(release_readiness, dict):
    require_condition(
        "conformance.release_readiness_schema",
        path=release_readiness_path,
        ok=release_readiness.get("schema") == "pi.e2e.release_readiness.v1",
        ok_msg="release-readiness schema matches",
        fail_msg=(
            "expected schema 'pi.e2e.release_readiness.v1', got "
            f"{release_readiness.get('schema')!r}"
        ),
        strict=True,
    )

    profiles = release_readiness.get("profiles")
    require_condition(
        "conformance.release_readiness_profiles_object",
        path=release_readiness_path,
        ok=isinstance(profiles, dict),
        ok_msg="profiles object exists",
        fail_msg="profiles must be an object",
        strict=True,
    )
    if isinstance(profiles, dict):
        for profile_name in ("safe", "balanced", "permissive"):
            profile_entry = profiles.get(profile_name)
            require_condition(
                f"conformance.release_readiness.{profile_name}.object",
                path=release_readiness_path,
                ok=isinstance(profile_entry, dict),
                ok_msg=f"{profile_name} profile entry is object",
                fail_msg=f"{profile_name} profile entry missing/invalid",
                strict=True,
            )
            if isinstance(profile_entry, dict):
                for field in (
                    "projected_pass_rate_pct",
                    "blocked_extensions",
                    "recommendation",
                ):
                    require_condition(
                        f"conformance.release_readiness.{profile_name}.{field}",
                        path=release_readiness_path,
                        ok=field in profile_entry,
                        ok_msg=f"{field} present",
                        fail_msg=f"missing {field}",
                        strict=True,
                    )

    workflow_outcomes = release_readiness.get("workflow_outcomes")
    require_condition(
        "conformance.release_readiness.workflow_outcomes_object",
        path=release_readiness_path,
        ok=isinstance(workflow_outcomes, dict),
        ok_msg="workflow_outcomes object exists",
        fail_msg="workflow_outcomes must be an object",
        strict=True,
    )
    if isinstance(workflow_outcomes, dict):
        require_condition(
            "conformance.release_readiness.workflow_outcomes.failing_suites_array",
            path=release_readiness_path,
            ok=isinstance(workflow_outcomes.get("failing_suites"), list),
            ok_msg="failing_suites is an array",
            fail_msg="failing_suites must be an array",
            strict=True,
        )
        require_condition(
            "conformance.release_readiness.workflow_outcomes.failing_unit_targets_array",
            path=release_readiness_path,
            ok=isinstance(workflow_outcomes.get("failing_unit_targets"), list),
            ok_msg="failing_unit_targets is an array",
            fail_msg="failing_unit_targets must be an array",
            strict=True,
        )

    known_risks = release_readiness.get("known_risks")
    require_condition(
        "conformance.release_readiness.known_risks_array",
        path=release_readiness_path,
        ok=isinstance(known_risks, list),
        ok_msg="known_risks is an array",
        fail_msg="known_risks must be an array",
        strict=True,
    )

    remediation = release_readiness.get("remediation")
    require_condition(
        "conformance.release_readiness.remediation_object",
        path=release_readiness_path,
        ok=isinstance(remediation, dict),
        ok_msg="remediation object exists",
        fail_msg="remediation must be an object",
        strict=True,
    )
    if isinstance(remediation, dict):
        require_condition(
            "conformance.release_readiness.remediation.immediate_array",
            path=release_readiness_path,
            ok=isinstance(remediation.get("immediate"), list),
            ok_msg="immediate remediation is an array",
            fail_msg="remediation.immediate must be an array",
            strict=True,
        )
        require_condition(
            "conformance.release_readiness.remediation.policy_guidance_array",
            path=release_readiness_path,
            ok=isinstance(remediation.get("policy_guidance"), list),
            ok_msg="policy_guidance is an array",
            fail_msg="remediation.policy_guidance must be an array",
            strict=True,
        )

release_readiness_md_path = artifact_dir / "release_readiness_summary.md"
readiness_md_exists = require_file(
    "conformance.release_readiness_markdown",
    release_readiness_md_path,
    strict=True,
    description="user-focused release-readiness markdown exists",
)
if readiness_md_exists:
    content = release_readiness_md_path.read_text(encoding="utf-8")
    require_condition(
        "conformance.release_readiness_markdown.known_risks_section",
        path=release_readiness_md_path,
        ok="## Known Risks" in content,
        ok_msg="contains Known Risks section",
        fail_msg="missing '## Known Risks' section",
        strict=True,
    )
    require_condition(
        "conformance.release_readiness_markdown.remediation_section",
        path=release_readiness_md_path,
        ok="## Recommended Remediation" in content,
        ok_msg="contains Recommended Remediation section",
        fail_msg="missing '## Recommended Remediation' section",
        strict=True,
    )

if isinstance(summary, dict):
    release_meta = summary.get("release_readiness")
    require_condition(
        "summary.release_readiness_object",
        path=summary_path,
        ok=isinstance(release_meta, dict),
        ok_msg="summary includes release_readiness metadata",
        fail_msg="summary missing release_readiness metadata",
        strict=True,
    )
    if isinstance(release_meta, dict):
        require_condition(
            "summary.release_readiness.path_matches",
            path=summary_path,
            ok=str(release_meta.get("path")) == str(release_readiness_path),
            ok_msg="summary.release_readiness.path matches artifact path",
            fail_msg=(
                "summary.release_readiness.path does not match "
                f"{release_readiness_path}"
            ),
            strict=True,
        )


# 6) Exception-policy coverage for non-pass conformance outcomes
full_conformance_report_path = reports_dir / "conformance" / "conformance_report.json"
full_conformance_report = load_json(
    "conformance.full_report_json",
    full_conformance_report_path,
    strict=strict_conformance,
)

baseline_path = reports_dir / "conformance_baseline.json"
baseline_payload = load_json(
    "conformance.baseline_json",
    baseline_path,
    strict=strict_conformance,
)

approved_exception_ids: set[str] = set()

if isinstance(baseline_payload, dict):
    exception_policy = baseline_payload.get("exception_policy")
    require_condition(
        "conformance.exception_policy.exists",
        path=baseline_path,
        ok=isinstance(exception_policy, dict),
        ok_msg="exception_policy object exists",
        fail_msg="missing exception_policy object in conformance_baseline.json",
        strict=strict_conformance,
    )

    if isinstance(exception_policy, dict):
        require_keys(
            "conformance.exception_policy",
            exception_policy,
            baseline_path,
            ["schema", "required_fields", "entries"],
            strict=strict_conformance,
        )

        policy_schema = exception_policy.get("schema")
        schema_ok = policy_schema == "pi.ext.exception_policy.v1"
        add_check(
            "conformance.exception_policy.schema",
            baseline_path,
            schema_ok,
            f"schema={policy_schema!r}"
            if schema_ok
            else f"unexpected schema {policy_schema!r}",
        )
        if not schema_ok:
            message = (
                "conformance.exception_policy.schema: expected "
                f"'pi.ext.exception_policy.v1', got {policy_schema!r}"
            )
            if strict_conformance:
                errors.append(message)
            else:
                warnings.append(message)

        required_fields = exception_policy.get("required_fields")
        required_fields_ok = (
            isinstance(required_fields, list)
            and all(isinstance(field, str) and field for field in required_fields)
        )
        require_condition(
            "conformance.exception_policy.required_fields",
            path=baseline_path,
            ok=required_fields_ok,
            ok_msg="required_fields is a non-empty string list",
            fail_msg="required_fields must be a list[str]",
            strict=strict_conformance,
        )

        entries = exception_policy.get("entries")
        entries_ok = isinstance(entries, list)
        require_condition(
            "conformance.exception_policy.entries_array",
            path=baseline_path,
            ok=entries_ok,
            ok_msg="entries is an array",
            fail_msg="entries must be an array",
            strict=strict_conformance,
        )

        if required_fields_ok and entries_ok:
            seen_ids: set[str] = set()
            today = datetime.now(timezone.utc).date()

            for idx, entry in enumerate(entries):
                check_prefix = f"conformance.exception_policy.entry[{idx}]"
                if not isinstance(entry, dict):
                    require_condition(
                        f"{check_prefix}.object",
                        path=baseline_path,
                        ok=False,
                        ok_msg="entry is object",
                        fail_msg="entry must be an object",
                        strict=strict_conformance,
                    )
                    continue

                missing_entry_fields = [
                    field for field in required_fields if field not in entry
                ]
                require_condition(
                    f"{check_prefix}.required_fields",
                    path=baseline_path,
                    ok=not missing_entry_fields,
                    ok_msg="entry contains all required fields",
                    fail_msg=f"entry missing required fields: {missing_entry_fields}",
                    strict=strict_conformance,
                )

                entry_id = str(entry.get("id", "")).strip()
                status_value = str(entry.get("status", "")).strip()
                review_by = str(entry.get("review_by", "")).strip()

                id_ok = bool(entry_id)
                require_condition(
                    f"{check_prefix}.id",
                    path=baseline_path,
                    ok=id_ok,
                    ok_msg=f"id={entry_id}",
                    fail_msg="id must be non-empty",
                    strict=strict_conformance,
                )

                if id_ok:
                    duplicate = entry_id in seen_ids
                    require_condition(
                        f"{check_prefix}.unique_id",
                        path=baseline_path,
                        ok=not duplicate,
                        ok_msg=f"id {entry_id} is unique",
                        fail_msg=f"duplicate exception id: {entry_id}",
                        strict=strict_conformance,
                    )
                    seen_ids.add(entry_id)

                status_ok = status_value in {"approved", "temporary"}
                require_condition(
                    f"{check_prefix}.status",
                    path=baseline_path,
                    ok=status_ok,
                    ok_msg=f"status={status_value}",
                    fail_msg=(
                        f"invalid status {status_value!r}; expected approved|temporary"
                    ),
                    strict=strict_conformance,
                )

                review_ok = False
                review_error = ""
                if review_by:
                    try:
                        review_date = datetime.strptime(review_by, "%Y-%m-%d").date()
                        review_ok = review_date >= today
                        if not review_ok:
                            review_error = (
                                f"review_by {review_by} is in the past (today={today})"
                            )
                    except ValueError:
                        review_error = f"review_by {review_by!r} is not YYYY-MM-DD"
                else:
                    review_error = "review_by is missing or empty"

                require_condition(
                    f"{check_prefix}.review_by",
                    path=baseline_path,
                    ok=review_ok,
                    ok_msg=f"review_by={review_by}",
                    fail_msg=review_error,
                    strict=strict_conformance,
                )

                if (
                    id_ok
                    and status_ok
                    and review_ok
                    and not missing_entry_fields
                ):
                    approved_exception_ids.add(entry_id)

if isinstance(full_conformance_report, dict):
    failures = full_conformance_report.get("failures")
    failures_ok = isinstance(failures, list)
    require_condition(
        "conformance.full_report_json.failures_array",
        path=full_conformance_report_path,
        ok=failures_ok,
        ok_msg="full report failures field is an array",
        fail_msg="full report failures field missing or not an array",
        strict=strict_conformance,
    )

    if failures_ok:
        failure_ids = sorted(
            {
                str(item.get("id", "")).strip()
                for item in failures
                if isinstance(item, dict) and str(item.get("id", "")).strip()
            }
        )
        missing_ids = sorted(
            [entry_id for entry_id in failure_ids if entry_id not in approved_exception_ids]
        )
        require_condition(
            "conformance.exception_policy.covers_failures",
            path=full_conformance_report_path,
            ok=not missing_ids,
            ok_msg=(
                f"all {len(failure_ids)} full-report failures covered by approved exceptions"
            ),
            fail_msg=f"missing approved exceptions for failures: {missing_ids}",
            strict=strict_conformance,
        )


status = "pass" if not errors else "fail"
contract_payload = {
    "schema": "pi.evidence.contract.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "profile": profile,
    "artifact_dir": str(artifact_dir),
    "status": status,
    "strict_conformance": strict_conformance,
    "checks": checks,
    "errors": errors,
    "warnings": warnings,
}
contract_file.parent.mkdir(parents=True, exist_ok=True)
contract_file.write_text(json.dumps(contract_payload, indent=2) + "\n", encoding="utf-8")

if isinstance(summary, dict):
    summary["evidence_contract"] = {
        "schema": "pi.evidence.contract.v1",
        "path": str(contract_file),
        "status": status,
        "strict_conformance": strict_conformance,
        "error_count": len(errors),
        "warning_count": len(warnings),
    }
    summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

if errors:
    print("EVIDENCE CONTRACT FAILED")
    for error in errors:
        print(f"- {error}")
    if warnings:
        print("\nEVIDENCE CONTRACT WARNINGS")
        for warning in warnings:
            print(f"  - {warning}")
    sys.exit(1)

print(
    "EVIDENCE CONTRACT PASSED: "
    f"{len(checks)} checks, {len(warnings)} warning(s), profile={profile}, strict_conformance={strict_conformance}"
)
sys.exit(0)
PY
    then
        echo "[contract] PASS ($contract_file)"
        return 0
    else
        echo "[contract] FAIL (see $contract_file)" >&2
        return 1
    fi
}

# ─── Main ─────────────────────────────────────────────────────────────────────

main() {
    echo "═══════════════════════════════════════════════════════════════"
    echo " Pi Agent Rust — Unified Verification Runner"
    echo " Timestamp: $TIMESTAMP"
    echo " Profile: $PROFILE"
    echo " Artifact dir: $ARTIFACT_DIR"
    echo " Lint: $(if $SKIP_LINT; then echo 'skip'; else echo 'enabled'; fi)"
    echo " Lib inline: enabled"
    echo " Integration targets: ${#SELECTED_UNIT_TARGETS[@]}"
    echo " E2E suites: ${#SELECTED_SUITES[@]}"
    if [[ -n "$RERUN_FROM" ]]; then
        echo " Rerun source: $RERUN_FROM"
    fi
    if [[ -n "$DIFF_FROM" ]]; then
        echo " Diff baseline: $DIFF_FROM"
    fi
    echo "═══════════════════════════════════════════════════════════════"
    echo ""

    capture_env

    local overall_exit=0

    # Phase 1: Lint gates (fmt + clippy).
    if ! run_lint_gates; then
        overall_exit=1
    fi

    # Phase 2: Build all selected targets.
    if ! build_tests; then
        echo "[fatal] Build failed, aborting test run." >&2
        exit 1
    fi

    # Phase 3: Lib inline tests.
    if ! run_lib_tests; then
        overall_exit=1
    fi

    # Phase 4: Integration targets (unit + vcr test files).
    for target in "${SELECTED_UNIT_TARGETS[@]}"; do
        if ! run_unit_target "$target"; then
            overall_exit=1
        fi
    done

    # Phase 5: E2E suites.
    for suite in "${SELECTED_SUITES[@]}"; do
        if [[ ! -f "tests/${suite}.rs" ]]; then
            echo "[skip] $suite: test file not found"
            continue
        fi
        if ! run_suite "$suite"; then
            overall_exit=1
        fi
    done

    write_summary

    if ! generate_extension_profile_matrix; then
        overall_exit=1
    fi

    if ! generate_soak_longevity_report; then
        overall_exit=1
    fi

    if ! generate_release_readiness_report; then
        overall_exit=1
    fi

    if ! generate_triage_diff; then
        overall_exit=1
    fi

    if ! validate_evidence_contract; then
        overall_exit=1
    fi

    exit $overall_exit
}

main "$@"
