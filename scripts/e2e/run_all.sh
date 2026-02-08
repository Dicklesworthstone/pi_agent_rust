#!/usr/bin/env bash
# scripts/e2e/run_all.sh — Unified verification runner (unit + E2E) with structured artifacts.
#
# Usage:
#   ./scripts/e2e/run_all.sh                              # profile=full (unit + all E2E suites)
#   ./scripts/e2e/run_all.sh --profile focused            # fast local loop
#   ./scripts/e2e/run_all.sh --profile ci                 # deterministic CI profile
#   ./scripts/e2e/run_all.sh --suite e2e_tui              # run specific E2E suite(s)
#   ./scripts/e2e/run_all.sh --unit-target node_http_shim # run specific unit target(s)
#   ./scripts/e2e/run_all.sh --rerun-from <summary.json>  # deterministic rerun of failed suites
#   ./scripts/e2e/run_all.sh --list                        # list available suites
#   ./scripts/e2e/run_all.sh --list-profiles              # list built-in profiles
#
# Environment:
#   E2E_ARTIFACT_DIR   Override artifact output directory (default: tests/e2e_results/<timestamp>)
#   E2E_PARALLELISM    Cargo test threads (default: 1 for determinism)
#   RUST_LOG           Log level for test harness (default: info)
#   VCR_MODE           Override VCR mode for all suites (default: unset, per-test decision)
#   VERIFY_PROFILE     Default profile when --profile is omitted (default: full)

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

# All known E2E test targets (discovered from tests/e2e_*.rs).
ALL_SUITES=(
    e2e_agent_loop
    e2e_cli
    e2e_cross_provider_parity
    e2e_extension_registration
    e2e_library_integration
    e2e_live
    e2e_live_harness
    e2e_message_session_control
    e2e_provider_streaming
    e2e_rpc
    e2e_session_persistence
    e2e_tools
    e2e_ts_extension_loading
    e2e_tui
)

ALL_UNIT_TARGETS=(
    ext_conformance_matrix
    node_buffer_shim
    node_crypto_shim
    node_http_shim
    npm_module_stubs
)

# ─── CLI Parsing ──────────────────────────────────────────────────────────────

SELECTED_SUITES=()
SELECTED_UNIT_TARGETS=()
LIST_ONLY=false
LIST_PROFILES=false
SKIP_UNIT=false

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
        --skip-unit)
            SKIP_UNIT=true
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
            echo "          [--rerun-from SUMMARY_JSON] [--skip-unit] [--list] [--list-profiles] [--help]"
            echo ""
            echo "Options:"
            echo "  --profile NAME       Verification profile: full | focused | ci"
            echo "  --suite NAME         Run only specified E2E suite(s) (repeatable)"
            echo "  --unit-target NAME   Run only specified unit target(s) (repeatable)"
            echo "  --rerun-from PATH    Rerun failed suites from prior summary.json"
            echo "  --skip-unit          Skip unit target execution"
            echo "  --list               List available E2E suites and exit"
            echo "  --list-profiles      List available verification profiles and exit"
            echo "  --help               Show this help"
            echo ""
            echo "Environment:"
            echo "  E2E_ARTIFACT_DIR     Artifact output directory"
            echo "  E2E_PARALLELISM      Cargo test threads (default: 1)"
            echo "  RUST_LOG             Log level (default: info)"
            echo "  VERIFY_PROFILE       Default profile when --profile not provided"
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
  full:
    unit targets: ${ALL_UNIT_TARGETS[*]}
    e2e suites:   ${ALL_SUITES[*]}
  focused:
    unit targets: ext_conformance_matrix node_buffer_shim node_crypto_shim
    e2e suites:   e2e_extension_registration e2e_tools
  ci:
    unit targets: ${ALL_UNIT_TARGETS[*]}
    e2e suites:   e2e_extension_registration
EOF
    exit 0
fi

if $LIST_ONLY; then
    echo "Available E2E suites:"
    for suite in "${ALL_SUITES[@]}"; do
        if [[ -f "tests/${suite}.rs" ]]; then
            echo "  $suite"
        else
            echo "  $suite (missing)"
        fi
    done
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
        *)
            echo "Unknown --profile value: $PROFILE (expected: full|focused|ci)" >&2
            exit 1
            ;;
    esac
fi

if [[ ${#SELECTED_UNIT_TARGETS[@]} -eq 0 && "$SKIP_UNIT" == false ]]; then
    case "$PROFILE" in
        full|ci)
            SELECTED_UNIT_TARGETS=("${ALL_UNIT_TARGETS[@]}")
            ;;
        focused)
            SELECTED_UNIT_TARGETS=(
                ext_conformance_matrix
                node_buffer_shim
                node_crypto_shim
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

RERUN_JSON_VALUE="null"
if [[ -n "$RERUN_FROM" ]]; then
    RERUN_JSON_VALUE="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$RERUN_FROM")"
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

    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo " Verification Summary (profile: $PROFILE)"
    echo " Unit targets: $passed_units/$total_units passed"
    echo " E2E suites:   $passed_suites/$total_suites passed"
    if [[ $failed_units -gt 0 ]]; then
        echo " Unit failed:  ${failed_unit_names[*]}"
    fi
    if [[ $failed_suites -gt 0 ]]; then
        echo " E2E failed:   ${failed_names[*]}"
    fi
    echo " Artifacts: $ARTIFACT_DIR"
    echo "═══════════════════════════════════════════════════════════════"
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

# ─── Main ─────────────────────────────────────────────────────────────────────

main() {
    echo "═══════════════════════════════════════════════════════════════"
    echo " Pi Agent Rust — Unified Verification Runner"
    echo " Timestamp: $TIMESTAMP"
    echo " Profile: $PROFILE"
    echo " Artifact dir: $ARTIFACT_DIR"
    echo " Unit targets: ${#SELECTED_UNIT_TARGETS[@]}"
    echo " E2E suites: ${#SELECTED_SUITES[@]}"
    if [[ -n "$RERUN_FROM" ]]; then
        echo " Rerun source: $RERUN_FROM"
    fi
    echo "═══════════════════════════════════════════════════════════════"
    echo ""

    capture_env

    if ! build_tests; then
        echo "[fatal] Build failed, aborting E2E run." >&2
        exit 1
    fi

    local overall_exit=0
    for target in "${SELECTED_UNIT_TARGETS[@]}"; do
        if ! run_unit_target "$target"; then
            overall_exit=1
        fi
    done

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

    exit $overall_exit
}

main "$@"
