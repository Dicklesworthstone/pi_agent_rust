#!/usr/bin/env bash
# scripts/e2e/run_snapcompact.sh — bd-cv653.7.6 snapcompact verification lanes.
#
# Lanes:
#   unit (always): pi::compaction_snap determinism/PNG/details/budget tests.
#   vcr  (always): capture-stub provider replay (vision receives frames,
#                  text-only model strips them with logged reason).
#   eval (opt-in): live-model retention QA. Requires PI_SNAPCOMPACT_EVAL=1 and
#                  a provider API key in env. Without it, the committed report
#                  is regenerated with status=NO_DATA (fail-closed) — the
#                  snapcompact mode stays default-off until a real eval lands.
#
# Usage:
#   ./scripts/e2e/run_snapcompact.sh                 # unit + vcr + report refresh
#   PI_SNAPCOMPACT_EVAL=1 ./scripts/e2e/run_snapcompact.sh --eval
#
# Artifacts:
#   tests/perf/reports/snapcompact_retention_eval.json

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REPORT_PATH="$PROJECT_ROOT/tests/perf/reports/snapcompact_retention_eval.json"
REPORT_SCHEMA="pi.compaction.snapcompact_eval.v1"
CORRELATION_ID="snapcompact-$(date -u +%Y%m%dT%H%M%SZ)-$$"

RUN_EVAL=false
[[ "${1:-}" == "--eval" ]] && RUN_EVAL=true

log_event() {
    # Structured JSONL event line: {ts, correlation_id, event, ...fields}
    printf '{"ts":"%s","correlation_id":"%s","event":"%s"%s}\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$CORRELATION_ID" "$1" "${2:-}" >&2
}

unit_lane() {
    log_event "lane_start" ',"lane":"unit"'
    (cd "$PROJECT_ROOT" && cargo test --locked --test snapcompact) 
    log_event "lane_end" ',"lane":"unit","status":"pass"'
}

vcr_lane() {
    log_event "lane_start" ',"lane":"vcr"'
    (cd "$PROJECT_ROOT" && cargo test --locked --test snapcompact_provider)
    log_event "lane_end" ',"lane":"vcr","status":"pass"'
}

eval_lane_status() {
    if [[ "$RUN_EVAL" == true ]]; then
        if [[ -n "${ANTHROPIC_API_KEY:-}" || -n "${OPENAI_API_KEY:-}" ]]; then
            echo "PENDING_IMPLEMENTATION"
        else
            echo "BLOCKED_NO_PROVIDER_KEY"
        fi
    else
        echo "NOT_REQUESTED"
    fi
}

main() {
    mkdir -p "$(dirname "$REPORT_PATH")"
    log_event "run_start" ",\"run_eval\":$RUN_EVAL"

    local unit_status="pass" vcr_status="pass"
    unit_lane || unit_status="fail"
    vcr_lane || vcr_status="fail"

    local eval_status
    eval_status="$(eval_lane_status)"

    # Retention recommendation is fail-closed: without a completed live
    # retention eval, snapcompact MUST remain default-off.
    local recommendation="keep_default_off_until_live_eval_passes"
    if [[ "$eval_status" == "PENDING_IMPLEMENTATION" ]]; then
        recommendation="keep_default_off_until_live_eval_passes"
    fi

    GENERATED_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)" python3 - "$REPORT_PATH" <<PY
import json, os, sys

report = {
    "schema": "$REPORT_SCHEMA",
    "generatedUtc": os.environ["GENERATED_UTC"],
    "correlationId": "$CORRELATION_ID",
    "bead": "bd-cv653.7.6",
    "lanes": {
        "unit": {"status": "$unit_status"},
        "vcr": {"status": "$vcr_status"},
        "retentionEval": {"status": "$eval_status"},
    },
    "status": "NO_DATA" if "$eval_status" != "PASS" else "OK",
    "recommendation": "$recommendation",
}
with open(sys.argv[1], "w") as f:
    json.dump(report, f, indent=2)
    f.write("\n")
print(f"wrote {sys.argv[1]}")
PY

    log_event "report_written" ",\"path\":\"$REPORT_PATH\""

    if [[ "$unit_status" != "pass" || "$vcr_status" != "pass" ]]; then
        log_event "run_end" ',"status":"fail"'
        exit 1
    fi
    log_event "run_end" ',"status":"pass"'
    echo "snapcompact lanes green; report: $REPORT_PATH"
}

main "$@"
