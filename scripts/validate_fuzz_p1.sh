#!/usr/bin/env bash
# validate_fuzz_p1.sh — Phase 1 Proptest Validation Suite
#
# Runs selected P1 proptest targets, captures per-test logs/timing, verifies
# aggregate case count >= 2000, and emits a structured JSON report.
#
# CPU-heavy operations run through `rch exec --` by default when `rch` is present.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REPORT_DIR="$PROJECT_ROOT/fuzz/reports"

usage() {
    cat <<'EOF'
Usage: ./scripts/validate_fuzz_p1.sh [OPTIONS]

Options:
  --min-cases=N        Minimum total generated cases required (default: 2000)
  --no-rch             Never use rch; run cargo locally
  --require-rch        Fail if rch is unavailable
  -h, --help           Show help

Examples:
  ./scripts/validate_fuzz_p1.sh
  ./scripts/validate_fuzz_p1.sh --min-cases=2500
  ./scripts/validate_fuzz_p1.sh --require-rch
EOF
}

is_positive_int() {
    case "$1" in
        ''|*[!0-9]*)
            return 1
            ;;
        *)
            [ "$1" -gt 0 ]
            ;;
    esac
}

resolve_shared_tmp_root() {
    local root="/data/tmp/pi_agent_rust/${USER:-agent}"
    local resolved=""

    if [ -e /data/tmp/pi_agent_rust ] && resolved="$(cd /data/tmp/pi_agent_rust && pwd -P 2>/dev/null)"; then
        case "$resolved" in
            "$PROJECT_ROOT"|"$PROJECT_ROOT"/*)
                root="/data/tmp/pi_agent_rust_cargo/${USER:-agent}"
                ;;
        esac
    fi

    printf '%s\n' "$root"
}

run_cmd() {
    if [ "$RCH_MODE" = "enabled" ]; then
        env "RCH_FORCE_REMOTE=${RCH_FORCE_REMOTE:-true}" rch exec -- "$@"
    else
        "$@"
    fi
}

supports_color() {
    [ -t 1 ] && [ "${NO_COLOR:-}" = "" ]
}

if supports_color; then
    C_RESET=$'\033[0m'
    C_RED=$'\033[31m'
    C_GREEN=$'\033[32m'
    C_YELLOW=$'\033[33m'
else
    C_RESET=""
    C_RED=""
    C_GREEN=""
    C_YELLOW=""
fi

# -------------------------------------------------------------------
# Parse arguments
# -------------------------------------------------------------------
MIN_CASES=2000
RCH_REQUEST="auto" # auto | always | never

for arg in "$@"; do
    case "$arg" in
        --min-cases=*)
            MIN_CASES="${arg#--min-cases=}"
            ;;
        --no-rch)
            RCH_REQUEST="never"
            ;;
        --require-rch)
            RCH_REQUEST="always"
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if ! is_positive_int "$MIN_CASES"; then
    echo "Invalid --min-cases value: '$MIN_CASES' (must be positive integer)" >&2
    exit 2
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is required but was not found on PATH" >&2
    exit 2
fi

RCH_AVAILABLE=0
if command -v rch >/dev/null 2>&1; then
    RCH_AVAILABLE=1
fi

case "$RCH_REQUEST" in
    always)
        if [ "$RCH_AVAILABLE" -eq 0 ]; then
            echo "--require-rch was set but rch is unavailable on PATH" >&2
            exit 2
        fi
        RCH_MODE="enabled"
        ;;
    never)
        RCH_MODE="disabled"
        ;;
    auto)
        if [ "$RCH_AVAILABLE" -eq 1 ]; then
            RCH_MODE="enabled"
        else
            RCH_MODE="fallback"
        fi
        ;;
    *)
        echo "Internal error: invalid RCH_REQUEST='$RCH_REQUEST'" >&2
        exit 2
        ;;
esac

mkdir -p "$REPORT_DIR"

TIMESTAMP_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
STAMP="$(date +%Y%m%d_%H%M%S)"
REPORT_FILE="$REPORT_DIR/p1_validation_${STAMP}.json"
SUITE_LOG="$REPORT_DIR/p1_validation_${STAMP}.log"
JSONL_FILE="$REPORT_DIR/p1_validation_${STAMP}.jsonl"

echo "=== FUZZ-V1 Phase 1 Validation Suite ==="
echo "Min cases required: $MIN_CASES"
echo "RCH mode: $RCH_MODE (request=$RCH_REQUEST, available=$RCH_AVAILABLE)"
echo "Report: $REPORT_FILE"
echo "Suite log: $SUITE_LOG"
echo "JSONL: $JSONL_FILE"
echo ""

if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    if [ -d /dev/shm ] && [ -w /dev/shm ]; then
        TARGET_ROOT="$(resolve_shared_tmp_root)"
    else
        TARGET_ROOT="$PROJECT_ROOT/.tmp/${USER:-agent}"
    fi
    export CARGO_TARGET_DIR="$TARGET_ROOT/p1_validate_${STAMP}_$$/target"
fi

if [ -z "${TMPDIR:-}" ]; then
    export TMPDIR="$(dirname "$CARGO_TARGET_DIR")/tmp"
fi

mkdir -p "$CARGO_TARGET_DIR" "$TMPDIR"

echo "CARGO_TARGET_DIR: $CARGO_TARGET_DIR"
echo "TMPDIR: $TMPDIR"
echo ""

# -------------------------------------------------------------------
# P1 Suite definition:
# issue|filter|cases_per_test
# -------------------------------------------------------------------
SUITE=(
    "bd-1nlkn|sse::tests::sse_chunking_invariant|256"
    "bd-hqnhx|tools::tests::proptest_normalize_for_match_invariants|64"
    "bd-34188|session::tests::proptest_session::session_entry_deser_never_panics|256"
    "bd-3667m|config::tests::proptest_config_deserializes_extension_risk_alpha_values|128"
    "bd-1be4i|model::tests::proptest_message_roundtrip_and_unknown_fields|256"
    "bd-3618n|providers::anthropic::tests::proptest_process_event::process_event_valid_never_panics|256"
    "bd-3618n|providers::openai::tests::proptest_process_event::process_event_valid_never_panics|256"
    "bd-3618n|providers::gemini::tests::proptest_process_event::process_event_valid_never_panics|256"
    "bd-25xci|vcr::tests::proptest_vcr::request_matches_never_panics|256"
    "bd-1y7y6|conformance::tests::proptest_compare_conformance_output_reflexive|128"
    "bd-3tb42|session_index::tests::proptest_index_session_snapshot_roundtrip_metadata|128"
    "bd-388hn|extensions::tests::concurrency::proptest_dispatch::events_dispatch_never_panics|512"
)

TOTAL_FUNCTIONS=${#SUITE[@]}
TOTAL_CASES_GENERATED=0
TOTAL_TIME_MS=0
FAILED=0
RESULTS_JSON=""
RESULTS_SEP=""

echo "Running $TOTAL_FUNCTIONS P1 proptest functions..."
echo ""

{
    echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] FUZZ-V1 start"
    echo "RCH_MODE=$RCH_MODE"
    echo "MIN_CASES=$MIN_CASES"
} > "$SUITE_LOG"

: > "$JSONL_FILE"

INDEX=0
for row in "${SUITE[@]}"; do
    INDEX=$((INDEX + 1))
    IFS='|' read -r issue_id test_filter cases_per_test <<< "$row"
    TEST_LOG="$REPORT_DIR/p1_${INDEX}_${STAMP}.log"

    echo ">>> [$INDEX/$TOTAL_FUNCTIONS] $test_filter (issue=$issue_id, cases=$cases_per_test)"
    START_NS=$(date +%s%N)
    run_cmd cargo test --lib "$test_filter" -- --nocapture 2>&1 | tee "$TEST_LOG"
    EXIT_CODE=${PIPESTATUS[0]}
    END_NS=$(date +%s%N)
    TIME_MS=$(( (END_NS - START_NS) / 1000000 ))
    TOTAL_TIME_MS=$((TOTAL_TIME_MS + TIME_MS))

    if [ "$EXIT_CODE" -eq 0 ]; then
        STATUS="pass"
        CASES_GENERATED="$cases_per_test"
        TOTAL_CASES_GENERATED=$((TOTAL_CASES_GENERATED + CASES_GENERATED))
    else
        STATUS="fail"
        CASES_GENERATED=0
        FAILED=$((FAILED + 1))
    fi

    SHRUNK_FAILURE=0
    if grep -q "minimal failing input" "$TEST_LOG"; then
        SHRUNK_FAILURE=1
    fi

    TEST_END_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    cat >> "$JSONL_FILE" <<EOFJSONL
{"timestamp":"$TEST_END_TS","issue":"$issue_id","function":"$test_filter","status":"$STATUS","cases":$CASES_GENERATED,"cases_configured":$cases_per_test,"exit_code":$EXIT_CODE,"time_ms":$TIME_MS,"shrunk_failure":$SHRUNK_FAILURE,"log_file":"fuzz/reports/$(basename "$TEST_LOG")"}
EOFJSONL

    printf '[%s] %s status=%s exit=%s time_ms=%s cases=%s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        "$test_filter" \
        "$STATUS" \
        "$EXIT_CODE" \
        "$TIME_MS" \
        "$CASES_GENERATED" >> "$SUITE_LOG"

    RESULTS_JSON="${RESULTS_JSON}${RESULTS_SEP}
    {
      \"issue\": \"$issue_id\",
      \"function\": \"$test_filter\",
      \"cases\": $CASES_GENERATED,
      \"cases_configured\": $cases_per_test,
      \"status\": \"$STATUS\",
      \"exit_code\": $EXIT_CODE,
      \"time_ms\": $TIME_MS,
      \"shrunk_failure\": $SHRUNK_FAILURE,
      \"log_file\": \"fuzz/reports/$(basename "$TEST_LOG")\"
    }"
    RESULTS_SEP=","
    if [ "$STATUS" = "pass" ]; then
        printf '%sPASS%s %s (cases=%s, time_ms=%s)\n' "$C_GREEN" "$C_RESET" "$test_filter" "$CASES_GENERATED" "$TIME_MS"
    else
        printf '%sFAIL%s %s (exit=%s, time_ms=%s, log=%s)\n' "$C_RED" "$C_RESET" "$test_filter" "$EXIT_CODE" "$TIME_MS" "fuzz/reports/$(basename "$TEST_LOG")"
    fi
    echo ""
done

PASS_COUNT=$((TOTAL_FUNCTIONS - FAILED))
ALL_PASS=0
if [ "$FAILED" -eq 0 ]; then
    ALL_PASS=1
fi

CASE_TARGET_MET=0
if [ "$TOTAL_CASES_GENERATED" -ge "$MIN_CASES" ]; then
    CASE_TARGET_MET=1
fi

cat > "$REPORT_FILE" <<EOFJSON
{
  "phase": "P1",
  "timestamp": "$TIMESTAMP_UTC",
  "rch_mode": "$RCH_MODE",
  "rch_request": "$RCH_REQUEST",
  "total_proptest_functions": $TOTAL_FUNCTIONS,
  "total_cases_generated": $TOTAL_CASES_GENERATED,
  "minimum_cases_required": $MIN_CASES,
  "all_pass": $ALL_PASS,
  "case_target_met": $CASE_TARGET_MET,
  "results": [${RESULTS_JSON}
  ],
  "summary": {
    "passed": $PASS_COUNT,
    "failed": $FAILED,
    "total_time_ms": $TOTAL_TIME_MS
  }
}
EOFJSON

echo "=== Summary ==="
echo "Functions run: $TOTAL_FUNCTIONS"
echo "Passed: $PASS_COUNT | Failed: $FAILED"
echo "Total cases generated: $TOTAL_CASES_GENERATED (required >= $MIN_CASES)"
echo "Total time: $((TOTAL_TIME_MS / 1000))s"
echo "Report: $REPORT_FILE"
echo "Suite log: $SUITE_LOG"
echo "JSONL: $JSONL_FILE"
echo ""

if [ "$ALL_PASS" -eq 1 ] && [ "$CASE_TARGET_MET" -eq 1 ]; then
    echo "${C_GREEN}RESULT: PASS${C_RESET}"
    exit 0
fi

if [ "$ALL_PASS" -ne 1 ]; then
    echo "${C_RED}RESULT: FAIL${C_RESET} (${C_YELLOW}one or more proptest functions failed${C_RESET})"
else
    echo "${C_RED}RESULT: FAIL${C_RESET} (${C_YELLOW}case target not met${C_RESET})"
fi
exit 1
