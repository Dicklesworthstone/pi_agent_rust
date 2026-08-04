#!/usr/bin/env bash
# scripts/ext_quality_pipeline.sh — Extension platform quality pipeline.
#
# Runs a focused set of quality checks for the extension runtime:
#   1. Format check (cargo fmt)
#   2. Clippy lint (lib + bin)
#   3. Cargo check (lib + bin + tests)
#   4. Extension unit tests (module-specific suites)
#   5. Extension integration tests (conformance, preflight, golden-path)
#   6. Preflight analyzer self-test
#
# Usage:
#   ./scripts/ext_quality_pipeline.sh                 # full pipeline
#   ./scripts/ext_quality_pipeline.sh --quick          # format + clippy + unit only
#   ./scripts/ext_quality_pipeline.sh --check-only     # format + clippy + check (no tests)
#   ./scripts/ext_quality_pipeline.sh --report         # write JSON report to stdout
#   ./scripts/ext_quality_pipeline.sh --require-rch    # require remote offload for heavy cargo steps
#   ./scripts/ext_quality_pipeline.sh --no-rch         # force local cargo execution
#
# Environment:
#   EXT_QP_PARALLELISM   Test threads (default: number of CPUs)
#   EXT_QP_TIMEOUT       Per-target timeout in seconds (default: 300)
#   EXT_QP_VERBOSE        Set to 1 for full cargo output (default: summary only)
#   EXT_QP_CARGO_RUNNER  Cargo runner mode: rch | auto | local (default: rch)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

# ─── Configuration ──────────────────────────────────────────────────────────

PARALLELISM="${EXT_QP_PARALLELISM:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)}"
TIMEOUT="${EXT_QP_TIMEOUT:-300}"
VERBOSE="${EXT_QP_VERBOSE:-0}"
MODE="full"
REPORT_JSON=0
CARGO_RUNNER_REQUEST="${EXT_QP_CARGO_RUNNER:-rch}" # rch | auto | local
CARGO_RUNNER_MODE="local"
declare -a CARGO_RUNNER_ARGS=("cargo")
SEEN_NO_RCH=false
SEEN_REQUIRE_RCH=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --quick)   MODE="quick"; shift ;;
        --check-only) MODE="check"; shift ;;
        --report)  REPORT_JSON=1; shift ;;
        --verbose) VERBOSE=1; shift ;;
        --no-rch)
            if [[ "$SEEN_REQUIRE_RCH" == true ]]; then
                echo "Cannot combine --no-rch and --require-rch" >&2
                exit 1
            fi
            SEEN_NO_RCH=true
            CARGO_RUNNER_REQUEST="local"
            shift
            ;;
        --require-rch)
            if [[ "$SEEN_NO_RCH" == true ]]; then
                echo "Cannot combine --require-rch and --no-rch" >&2
                exit 1
            fi
            SEEN_REQUIRE_RCH=true
            CARGO_RUNNER_REQUEST="rch"
            shift
            ;;
        --help|-h)
            head -20 "$0" | grep '^#' | sed 's/^# \?//'
            exit 0
            ;;
        *) echo "Unknown flag: $1"; exit 1 ;;
    esac
done

# ─── Cargo Runner Resolution ────────────────────────────────────────────────

if [[ "$CARGO_RUNNER_REQUEST" != "rch" && "$CARGO_RUNNER_REQUEST" != "auto" && "$CARGO_RUNNER_REQUEST" != "local" ]]; then
    echo "Invalid EXT_QP_CARGO_RUNNER value: $CARGO_RUNNER_REQUEST (expected: rch|auto|local)" >&2
    exit 2
fi

if [[ "$CARGO_RUNNER_REQUEST" == "rch" ]]; then
    if ! command -v rch >/dev/null 2>&1; then
        echo "EXT_QP_CARGO_RUNNER=rch requested, but 'rch' is not available in PATH." >&2
        exit 2
    fi
    if ! rch check --quiet >/dev/null 2>&1; then
        echo "'rch check' failed; refusing heavy local cargo fallback. Fix rch or pass --no-rch." >&2
        exit 2
    fi
    CARGO_RUNNER_MODE="rch"
    CARGO_RUNNER_ARGS=("rch" "exec" "--" "cargo")
elif [[ "$CARGO_RUNNER_REQUEST" == "auto" ]] && command -v rch >/dev/null 2>&1; then
    if rch check --quiet >/dev/null 2>&1; then
        CARGO_RUNNER_MODE="rch"
        CARGO_RUNNER_ARGS=("rch" "exec" "--" "cargo")
    else
        echo "rch detected but unhealthy; auto mode will run cargo locally (set --require-rch to fail fast)." >&2
    fi
fi

# ─── State tracking ─────────────────────────────────────────────────────────

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
declare -a RESULTS=()
START_TIME=$(date +%s)

log() {
    if [[ "$REPORT_JSON" -eq 0 ]]; then
        echo "[$1] $2"
    fi
}

run_step() {
    local name="$1"
    shift
    local step_start
    step_start=$(date +%s)

    log "RUN" "$name"
    local output
    local exit_code=0
    if [[ "$VERBOSE" -eq 1 ]]; then
        if [[ "$REPORT_JSON" -eq 1 ]]; then
            "$@" >&2 || exit_code=$?
        else
            "$@" 2>&1 || exit_code=$?
        fi
    else
        output=$("$@" 2>&1) || exit_code=$?
    fi
    local step_end
    step_end=$(date +%s)
    local elapsed=$((step_end - step_start))

    if [[ $exit_code -eq 0 ]]; then
        log "PASS" "$name (${elapsed}s)"
        PASS_COUNT=$((PASS_COUNT + 1))
        RESULTS+=("{\"name\":\"$name\",\"status\":\"pass\",\"seconds\":$elapsed}")
    else
        log "FAIL" "$name (${elapsed}s, exit=$exit_code)"
        if [[ "$VERBOSE" -eq 0 ]] && [[ -n "${output:-}" ]]; then
            # Show last 20 lines on failure.
            printf '%s\n' "$output" | tail -20 >&2
        fi
        FAIL_COUNT=$((FAIL_COUNT + 1))
        RESULTS+=("{\"name\":\"$name\",\"status\":\"fail\",\"seconds\":$elapsed,\"exit_code\":$exit_code}")
    fi
}

run_compile_step() {
    local name="$1"
    shift
    run_step "$name" "${CARGO_RUNNER_ARGS[@]}" "$@"
}

skip_step() {
    local name="$1"
    log "SKIP" "$name"
    SKIP_COUNT=$((SKIP_COUNT + 1))
    RESULTS+=("{\"name\":\"$name\",\"status\":\"skip\",\"seconds\":0}")
}

# ─── Extension test targets ─────────────────────────────────────────────────
# These are the test files specifically related to the extension platform.

# Unit-level extension tests (from suite_classification.toml vcr suite).
EXT_UNIT_TARGETS=(
    extensions_registration
    extensions_message_session
    extensions_provider_streaming
    extensions_provider_oauth
    extensions_event_wiring
    extensions_event_cancellation
    extensions_reliability
    extensions_stress
    extensions_fs_shim
    extensions_process_shim
    extensions_url_shim
    extensions_manifest
    extensions_policy_negative
    extensions_concurrent_correctness
    ext_proptest
    ext_preflight_analyzer
    node_fs_shim
    node_crypto_shim
    node_child_process_shim
    node_buffer_shim
    node_events_shim
    node_http_shim
    node_shim_integration
    node_bun_api_matrix
    npm_module_stubs
    lab_runtime_extensions
)

# Integration/E2E extension tests.
EXT_INTEGRATION_TARGETS=(
    e2e_extension_registration
    e2e_message_session_control
    e2e_provider_streaming
    e2e_ts_extension_loading
    e2e_workflow_preflight
    e2e_golden_path
)

# Conformance tests.
EXT_CONFORMANCE_TARGETS=(
    ext_conformance
    ext_conformance_scenarios
    ext_conformance_matrix
    ext_conformance_shapes
    ext_conformance_guard
)

# ─── Pipeline stages ────────────────────────────────────────────────────────

log "INFO" "cargo runner: $CARGO_RUNNER_MODE (request=$CARGO_RUNNER_REQUEST)"

# Stage 1: Format check
run_step "cargo-fmt" cargo fmt --check

# Stage 2: Clippy
run_compile_step "clippy-lib" clippy --locked --lib -- -D warnings
run_compile_step "clippy-bin" clippy --locked --bin pi -- -D warnings

# Stage 3: Cargo check (catches compilation errors in test files). The
# ext_conformance target executes the internal legacy-capture binary, so the
# repository-only feature must be explicit even though end-user installs leave
# it disabled.
run_compile_step "cargo-check-tests" check --locked --tests --features internal-legacy-capture

if [[ "$MODE" == "check" ]]; then
    for target in "${EXT_UNIT_TARGETS[@]}" "${EXT_INTEGRATION_TARGETS[@]}" "${EXT_CONFORMANCE_TARGETS[@]}"; do
        skip_step "test:$target"
    done
else
    # Stage 4: Extension unit tests
    for target in "${EXT_UNIT_TARGETS[@]}"; do
        if [[ "$MODE" == "quick" ]] && [[ ${#RESULTS[@]} -gt 15 ]]; then
            skip_step "test:$target"
            continue
        fi
        run_step "test:$target" timeout "$TIMEOUT" "${CARGO_RUNNER_ARGS[@]}" test --locked --test "$target" -- --test-threads="$PARALLELISM"
    done

    if [[ "$MODE" != "quick" ]]; then
        # Stage 5: Extension integration tests
        for target in "${EXT_INTEGRATION_TARGETS[@]}"; do
            run_step "test:$target" timeout "$TIMEOUT" "${CARGO_RUNNER_ARGS[@]}" test --locked --test "$target" -- --test-threads="$PARALLELISM"
        done

        # Stage 6: Conformance tests
        for target in "${EXT_CONFORMANCE_TARGETS[@]}"; do
            if [[ "$target" == "ext_conformance" ]]; then
                run_step "test:$target" timeout "$TIMEOUT" "${CARGO_RUNNER_ARGS[@]}" test --locked --test "$target" --features internal-legacy-capture -- --test-threads="$PARALLELISM"
            else
                run_step "test:$target" timeout "$TIMEOUT" "${CARGO_RUNNER_ARGS[@]}" test --locked --test "$target" -- --test-threads="$PARALLELISM"
            fi
        done
    else
        for target in "${EXT_INTEGRATION_TARGETS[@]}" "${EXT_CONFORMANCE_TARGETS[@]}"; do
            skip_step "test:$target"
        done
    fi

    # Stage 7: Inline extension module tests
    run_step "test:lib-extension-preflight" "${CARGO_RUNNER_ARGS[@]}" test --locked --lib extension_preflight -- --test-threads="$PARALLELISM"
fi

# ─── Summary ────────────────────────────────────────────────────────────────

END_TIME=$(date +%s)
TOTAL_ELAPSED=$((END_TIME - START_TIME))
TOTAL=$((PASS_COUNT + FAIL_COUNT + SKIP_COUNT))

if [[ "$REPORT_JSON" -eq 1 ]]; then
    # Build JSON array from RESULTS.
    JSON_RESULTS=""
    for r in "${RESULTS[@]}"; do
        if [[ -n "$JSON_RESULTS" ]]; then
            JSON_RESULTS="$JSON_RESULTS,$r"
        else
            JSON_RESULTS="$r"
        fi
    done

    VERDICT="pass"
    if [[ $FAIL_COUNT -gt 0 ]]; then
        VERDICT="fail"
    fi

    cat <<EOF
{
  "schema": "pi.ext_quality_pipeline.v1",
  "mode": "$MODE",
  "cargo_runner_request": "$CARGO_RUNNER_REQUEST",
  "cargo_runner_mode": "$CARGO_RUNNER_MODE",
  "verdict": "$VERDICT",
  "total_seconds": $TOTAL_ELAPSED,
  "counts": {
    "pass": $PASS_COUNT,
    "fail": $FAIL_COUNT,
    "skip": $SKIP_COUNT,
    "total": $TOTAL
  },
  "steps": [$JSON_RESULTS]
}
EOF
    if [[ $FAIL_COUNT -gt 0 ]]; then
        exit 1
    fi
else
    echo ""
    echo "═══════════════════════════════════════════════════════════"
    echo "  Extension Quality Pipeline — ${MODE^^} mode"
    echo "═══════════════════════════════════════════════════════════"
    echo "  Cargo runner: $CARGO_RUNNER_MODE (request=$CARGO_RUNNER_REQUEST)"
    echo "  Pass: $PASS_COUNT  Fail: $FAIL_COUNT  Skip: $SKIP_COUNT  Total: $TOTAL"
    echo "  Duration: ${TOTAL_ELAPSED}s"
    echo "═══════════════════════════════════════════════════════════"

    if [[ $FAIL_COUNT -gt 0 ]]; then
        echo "  VERDICT: FAIL"
        exit 1
    else
        echo "  VERDICT: PASS"
    fi
fi
