#!/usr/bin/env bash
# scripts/release_gate.sh — Release gate requiring conformance evidence bundle.
#
# Validates that all required evidence artifacts exist and meet thresholds
# before allowing a release. Designed to run as a CI step or local pre-release
# check.
#
# Usage:
#   ./scripts/release_gate.sh                          # check latest evidence
#   ./scripts/release_gate.sh --evidence-dir <path>    # check specific run
#   ./scripts/release_gate.sh --report                 # JSON output
#
# Environment:
#   RELEASE_GATE_MIN_PASS_RATE     Minimum conformance pass rate (default: 80)
#   RELEASE_GATE_MAX_FAIL_COUNT    Maximum conformance failures (default: 36)
#   RELEASE_GATE_MAX_NA_COUNT      Maximum N/A scenarios (default: 170)
#   RELEASE_GATE_REQUIRE_PREFLIGHT Set to 1 to require preflight analyzer (default: 0)
#   RELEASE_GATE_REQUIRE_QUALITY   Set to 1 to require quality pipeline pass (default: 0)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ─── Configuration ──────────────────────────────────────────────────────────

MIN_PASS_RATE="${RELEASE_GATE_MIN_PASS_RATE:-80}"
MAX_FAIL_COUNT="${RELEASE_GATE_MAX_FAIL_COUNT:-36}"
MAX_NA_COUNT="${RELEASE_GATE_MAX_NA_COUNT:-170}"
REQUIRE_PREFLIGHT="${RELEASE_GATE_REQUIRE_PREFLIGHT:-0}"
REQUIRE_QUALITY="${RELEASE_GATE_REQUIRE_QUALITY:-0}"
EVIDENCE_DIR=""
REPORT_JSON=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --evidence-dir) EVIDENCE_DIR="$2"; shift 2 ;;
        --report) REPORT_JSON=1; shift ;;
        --help|-h)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *) echo "Unknown flag: $1"; exit 1 ;;
    esac
done

# Auto-detect latest evidence directory if not specified.
if [[ -z "$EVIDENCE_DIR" ]]; then
    E2E_RESULTS="$PROJECT_ROOT/tests/e2e_results"
    if [[ -d "$E2E_RESULTS" ]]; then
        EVIDENCE_DIR=$(ls -d "$E2E_RESULTS"/*/ 2>/dev/null | sort | tail -1)
    fi
fi

# ─── State tracking ─────────────────────────────────────────────────────────

PASS_COUNT=0
FAIL_COUNT=0
WARN_COUNT=0
declare -a CHECKS=()

log() {
    if [[ "$REPORT_JSON" -eq 0 ]]; then
        echo "[$1] $2"
    fi
}

check_pass() {
    local name="$1"
    local detail="$2"
    log "PASS" "$name: $detail"
    PASS_COUNT=$((PASS_COUNT + 1))
    CHECKS+=("{\"name\":\"$name\",\"status\":\"pass\",\"detail\":\"$detail\"}")
}

check_fail() {
    local name="$1"
    local detail="$2"
    log "FAIL" "$name: $detail"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    CHECKS+=("{\"name\":\"$name\",\"status\":\"fail\",\"detail\":\"$detail\"}")
}

check_warn() {
    local name="$1"
    local detail="$2"
    log "WARN" "$name: $detail"
    WARN_COUNT=$((WARN_COUNT + 1))
    CHECKS+=("{\"name\":\"$name\",\"status\":\"warn\",\"detail\":\"$detail\"}")
}

# ─── Gate checks ────────────────────────────────────────────────────────────

# Gate 1: Evidence directory exists
if [[ -z "$EVIDENCE_DIR" ]] || [[ ! -d "$EVIDENCE_DIR" ]]; then
    check_fail "evidence_dir" "No evidence directory found"
else
    check_pass "evidence_dir" "Found: $EVIDENCE_DIR"
fi

# Gate 2: Evidence contract
EVIDENCE_CONTRACT="$EVIDENCE_DIR/evidence_contract.json"
if [[ -f "$EVIDENCE_CONTRACT" ]]; then
    CONTRACT_STATUS=$(python3 -c "
import json
with open('$EVIDENCE_CONTRACT') as f:
    data = json.load(f)
print(data.get('status', 'unknown'))
" 2>/dev/null || echo "parse_error")

    if [[ "$CONTRACT_STATUS" == "pass" ]]; then
        check_pass "evidence_contract" "status=pass"
    elif [[ "$CONTRACT_STATUS" == "parse_error" ]]; then
        check_fail "evidence_contract" "Failed to parse evidence_contract.json"
    else
        check_fail "evidence_contract" "status=$CONTRACT_STATUS (expected pass)"
    fi
else
    check_fail "evidence_contract" "evidence_contract.json not found"
fi

# Gate 3: Conformance summary
CONFORMANCE_DIR="$PROJECT_ROOT/tests/ext_conformance/reports"
CONFORMANCE_SUMMARY="$CONFORMANCE_DIR/conformance_summary.json"
if [[ -f "$CONFORMANCE_SUMMARY" ]]; then
    SUMMARY_DATA=$(python3 -c "
import json
with open('$CONFORMANCE_SUMMARY') as f:
    data = json.load(f)
counts = data.get('counts', {})
print(f\"{counts.get('total', 0)} {counts.get('pass', 0)} {counts.get('fail', 0)} {counts.get('na', 0)} {data.get('pass_rate_pct', 0)}\")
" 2>/dev/null || echo "0 0 0 0 0")

    read -r TOTAL PASS FAIL NA PASS_RATE <<< "$SUMMARY_DATA"

    if [[ "$TOTAL" -eq 0 ]]; then
        check_fail "conformance_total" "Zero total scenarios in conformance summary"
    else
        check_pass "conformance_total" "$TOTAL total scenarios"
    fi

    # Pass rate threshold
    PASS_RATE_INT="${PASS_RATE%.*}"
    if [[ "$PASS_RATE_INT" -ge "$MIN_PASS_RATE" ]]; then
        check_pass "conformance_pass_rate" "${PASS_RATE}% >= ${MIN_PASS_RATE}% threshold"
    else
        check_fail "conformance_pass_rate" "${PASS_RATE}% < ${MIN_PASS_RATE}% threshold"
    fi

    # Fail count threshold
    if [[ "$FAIL" -le "$MAX_FAIL_COUNT" ]]; then
        check_pass "conformance_fail_count" "$FAIL failures <= $MAX_FAIL_COUNT threshold"
    else
        check_fail "conformance_fail_count" "$FAIL failures > $MAX_FAIL_COUNT threshold"
    fi

    # N/A count threshold
    if [[ "$NA" -le "$MAX_NA_COUNT" ]]; then
        check_pass "conformance_na_count" "$NA N/A <= $MAX_NA_COUNT threshold"
    else
        check_fail "conformance_na_count" "$NA N/A > $MAX_NA_COUNT threshold"
    fi
else
    check_fail "conformance_summary" "conformance_summary.json not found"
fi

# Gate 4: Conformance report
CONFORMANCE_REPORT="$CONFORMANCE_DIR/CONFORMANCE_REPORT.md"
if [[ -f "$CONFORMANCE_REPORT" ]]; then
    check_pass "conformance_report" "CONFORMANCE_REPORT.md exists"
else
    check_warn "conformance_report" "CONFORMANCE_REPORT.md not found (optional)"
fi

# Gate 5: Conformance baseline
CONFORMANCE_BASELINE="$CONFORMANCE_DIR/conformance_baseline.json"
if [[ -f "$CONFORMANCE_BASELINE" ]]; then
    check_pass "conformance_baseline" "Baseline exists for regression checks"
else
    check_warn "conformance_baseline" "No baseline (first run?)"
fi

# Gate 6: Compilation check (cargo check)
if cargo check --lib --quiet 2>/dev/null; then
    check_pass "cargo_check" "Library compiles cleanly"
else
    check_fail "cargo_check" "cargo check --lib failed"
fi

# Gate 7: Clippy lint
if cargo clippy --lib -- -D warnings --quiet 2>/dev/null; then
    check_pass "clippy" "No clippy warnings"
else
    check_fail "clippy" "Clippy has warnings"
fi

# Gate 8: Preflight analyzer (optional)
if [[ "$REQUIRE_PREFLIGHT" -eq 1 ]]; then
    if cargo test --lib extension_preflight --quiet 2>/dev/null; then
        check_pass "preflight_tests" "Extension preflight tests pass"
    else
        check_fail "preflight_tests" "Extension preflight tests failed"
    fi
fi

# Gate 9: Quality pipeline (optional)
if [[ "$REQUIRE_QUALITY" -eq 1 ]]; then
    if "$SCRIPT_DIR/ext_quality_pipeline.sh" --check-only --report >/dev/null 2>&1; then
        check_pass "quality_pipeline" "Extension quality pipeline passes"
    else
        check_fail "quality_pipeline" "Extension quality pipeline failed"
    fi
fi

# Gate 10: Suite classification guard
CLASSIFICATION="$PROJECT_ROOT/tests/suite_classification.toml"
if [[ -f "$CLASSIFICATION" ]]; then
    check_pass "suite_classification" "suite_classification.toml exists"
else
    check_fail "suite_classification" "suite_classification.toml missing"
fi

# Gate 11: Traceability matrix
TRACEABILITY="$PROJECT_ROOT/docs/traceability_matrix.json"
if [[ -f "$TRACEABILITY" ]]; then
    check_pass "traceability_matrix" "traceability_matrix.json exists"
else
    check_warn "traceability_matrix" "traceability_matrix.json not found"
fi

# ─── Summary ────────────────────────────────────────────────────────────────

TOTAL_CHECKS=$((PASS_COUNT + FAIL_COUNT + WARN_COUNT))

if [[ "$REPORT_JSON" -eq 1 ]]; then
    JSON_CHECKS=""
    for c in "${CHECKS[@]}"; do
        if [[ -n "$JSON_CHECKS" ]]; then
            JSON_CHECKS="$JSON_CHECKS,$c"
        else
            JSON_CHECKS="$c"
        fi
    done

    VERDICT="pass"
    if [[ $FAIL_COUNT -gt 0 ]]; then
        VERDICT="fail"
    fi

    cat <<EOF
{
  "schema": "pi.release_gate.v1",
  "verdict": "$VERDICT",
  "thresholds": {
    "min_pass_rate": $MIN_PASS_RATE,
    "max_fail_count": $MAX_FAIL_COUNT,
    "max_na_count": $MAX_NA_COUNT
  },
  "counts": {
    "pass": $PASS_COUNT,
    "fail": $FAIL_COUNT,
    "warn": $WARN_COUNT,
    "total": $TOTAL_CHECKS
  },
  "checks": [$JSON_CHECKS]
}
EOF
else
    echo ""
    echo "═══════════════════════════════════════════════════════════"
    echo "  Release Gate — Conformance Evidence Bundle"
    echo "═══════════════════════════════════════════════════════════"
    echo "  Pass: $PASS_COUNT  Fail: $FAIL_COUNT  Warn: $WARN_COUNT  Total: $TOTAL_CHECKS"
    echo "  Thresholds: pass_rate>=${MIN_PASS_RATE}%, fail<=${MAX_FAIL_COUNT}, na<=${MAX_NA_COUNT}"
    echo "═══════════════════════════════════════════════════════════"

    if [[ $FAIL_COUNT -gt 0 ]]; then
        echo "  VERDICT: FAIL — release blocked"
        exit 1
    else
        echo "  VERDICT: PASS — release approved"
    fi
fi
