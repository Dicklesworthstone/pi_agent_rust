#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/persistence-fault-injection/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-persistence-fault-injection-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"

default_build_root() {
    local base="/data/tmp/pi_agent_rust"
    local resolved=""

    if [[ -e "$base" ]] && resolved="$(cd "$base" && pwd -P 2>/dev/null)"; then
        case "$resolved" in
            "$PROJECT_ROOT"|"$PROJECT_ROOT"/*)
                base="/data/tmp/pi_agent_rust_cargo"
                ;;
        esac
    fi

    printf '%s\n' "$base"
}

AGENT_SUFFIX="${PERSISTENCE_AGENT_SUFFIX:-${CODEX_THREAD_ID:-${USER:-agent}}}"
BUILD_ROOT="$(default_build_root)"
if [[ -z "${CARGO_TARGET_DIR:-}" || "${CARGO_TARGET_DIR:-}" == "target" ]]; then
    export CARGO_TARGET_DIR="$BUILD_ROOT/$AGENT_SUFFIX/target"
fi
if [[ -z "${TMPDIR:-}" || "${TMPDIR:-}" == "/tmp" || "${TMPDIR:-}" == "/data/tmp" ]]; then
    export TMPDIR="$BUILD_ROOT/$AGENT_SUFFIX/tmp"
fi
mkdir -p "$CARGO_TARGET_DIR" "$TMPDIR"

MIN_REPO_FREE_MB="${PERSISTENCE_MIN_REPO_FREE_MB:-2048}"
MIN_TMP_FREE_MB="${PERSISTENCE_MIN_TMP_FREE_MB:-8192}"

CARGO_RUNNER_MODE="${PERSISTENCE_CARGO_RUNNER:-rch}"
PERSISTENCE_RCH_FORCE_REMOTE="${PERSISTENCE_RCH_FORCE_REMOTE:-true}"
if [[ "$CARGO_RUNNER_MODE" == "rch" ]]; then
    PERSISTENCE_RCH_REQUIRE_REMOTE="${PERSISTENCE_RCH_REQUIRE_REMOTE:-true}"
else
    PERSISTENCE_RCH_REQUIRE_REMOTE="${PERSISTENCE_RCH_REQUIRE_REMOTE:-false}"
fi
declare -a CARGO_RUNNER_PREFIX=()

# RCH retrieves these conventional top-level test reports after `cargo test`.
# The runner moves them into the case directory immediately after each command.
RCH_TEST_LOG_REPORT="junit.xml"
RCH_ARTIFACT_INDEX_REPORT="test-results.xml"

available_mb() {
    local path="$1"
    df -Pm "$path" | awk 'NR == 2 { print $4 }'
}

assert_free_mb() {
    local path="$1"
    local min_mb="$2"
    local label="$3"
    local free_mb
    free_mb="$(available_mb "$path")"
    if [[ -z "$free_mb" || "$free_mb" -lt "$min_mb" ]]; then
        echo "[fault-injection] Insufficient free space for $label: ${free_mb:-unknown}MB available, requires >= ${min_mb}MB (path: $path)" >&2
        return 1
    fi
    echo "[fault-injection] Free space $label: ${free_mb}MB (path: $path)"
}

append_rch_env_allowlist() {
    local key
    for key in \
        CI_CORRELATION_ID \
        RUST_LOG \
        TEST_LOG_JSONL_PATH \
        TEST_ARTIFACT_INDEX_PATH
    do
        case ",${RCH_ENV_ALLOWLIST:-}," in
            *",$key,"*) ;;
            *)
                if [[ -n "${RCH_ENV_ALLOWLIST:-}" ]]; then
                    RCH_ENV_ALLOWLIST="$RCH_ENV_ALLOWLIST,$key"
                else
                    RCH_ENV_ALLOWLIST="$key"
                fi
                ;;
        esac
    done
    export RCH_ENV_ALLOWLIST
}

configure_cargo_runner() {
    case "$CARGO_RUNNER_MODE" in
        rch)
            if ! command -v rch >/dev/null 2>&1; then
                echo "PERSISTENCE_CARGO_RUNNER=rch requested, but 'rch' is not available in PATH." >&2
                exit 1
            fi
            CARGO_RUNNER_PREFIX=("rch" "exec" "--")
            append_rch_env_allowlist
            ;;
        auto)
            if command -v rch >/dev/null 2>&1; then
                CARGO_RUNNER_PREFIX=("rch" "exec" "--")
                append_rch_env_allowlist
            else
                CARGO_RUNNER_PREFIX=()
            fi
            ;;
        local)
            CARGO_RUNNER_PREFIX=()
            ;;
        *)
            echo "Unknown PERSISTENCE_CARGO_RUNNER value: $CARGO_RUNNER_MODE (expected: rch|auto|local)" >&2
            exit 1
            ;;
    esac
}

run_cargo() {
    if [[ ${#CARGO_RUNNER_PREFIX[@]} -eq 0 ]]; then
        cargo "$@"
    else
        env \
            "RCH_FORCE_REMOTE=$PERSISTENCE_RCH_FORCE_REMOTE" \
            "RCH_REQUIRE_REMOTE=$PERSISTENCE_RCH_REQUIRE_REMOTE" \
            "${CARGO_RUNNER_PREFIX[@]}" cargo "$@"
    fi
}

write_case_result() {
    local result_file="$1"
    local case_id="$2"
    local test_name="$3"
    local exit_code="$4"
    local duration_ms="$5"
    local log_file="$6"
    local test_log="$7"
    local artifact_index="$8"
    local feature_name="${9:-}"

    cat >"$result_file" <<EOF
{
  "schema": "pi.e2e.persistence_fault_case.v1",
  "correlation_id": "$CORRELATION_ID",
  "case_id": "$case_id",
  "suite": "e2e_session_persistence",
  "test_name": "$test_name",
  "feature": "$feature_name",
  "exit_code": $exit_code,
  "duration_ms": $duration_ms,
  "log_file": "$log_file",
  "test_log_jsonl": "$test_log",
  "artifact_index_jsonl": "$artifact_index",
  "timestamp": "$STAMP"
}
EOF
}

run_case() {
    local case_id="$1"
    local test_name="$2"
    local feature_name="${3:-}"
    local case_dir="$ARTIFACT_DIR/$case_id"
    local log_file="$case_dir/output.log"
    local result_file="$case_dir/result.json"
    local test_log="$case_dir/test-log.jsonl"
    local artifact_index="$case_dir/artifact-index.jsonl"
    local harness_test_log="$test_log"
    local harness_artifact_index="$artifact_index"
    local start_epoch end_epoch duration_ms exit_code diagnostics_exit

    mkdir -p "$case_dir"
    if [[ ${#CARGO_RUNNER_PREFIX[@]} -gt 0 ]]; then
        harness_test_log="$RCH_TEST_LOG_REPORT"
        harness_artifact_index="$RCH_ARTIFACT_INDEX_REPORT"
        if [[ -e "$PROJECT_ROOT/$harness_test_log" || -e "$PROJECT_ROOT/$harness_artifact_index" ]]; then
            echo "[fault-injection] Refusing to overwrite pre-existing RCH test reports in $PROJECT_ROOT" >&2
            write_case_result \
                "$result_file" \
                "$case_id" \
                "$test_name" \
                68 \
                0 \
                "$log_file" \
                "$test_log" \
                "$artifact_index" \
                "$feature_name"
            return 68
        fi
    fi
    export TEST_LOG_JSONL_PATH="$harness_test_log"
    export TEST_ARTIFACT_INDEX_PATH="$harness_artifact_index"

    echo "[fault-injection] Running case '$case_id' ($test_name)"
    start_epoch=$(date +%s%N 2>/dev/null || date +%s)

    set +e
    if [[ -n "$feature_name" ]]; then
        run_cargo test \
            --features "$feature_name" \
            --test e2e_session_persistence \
            "$test_name" \
            -- \
            --nocapture \
            --test-threads=1 \
            2>&1 | tee "$log_file"
    else
        run_cargo test \
            --test e2e_session_persistence \
            "$test_name" \
            -- \
            --nocapture \
            --test-threads=1 \
            2>&1 | tee "$log_file"
    fi
    exit_code=${PIPESTATUS[0]}
    set -e

    diagnostics_exit=0
    if [[ ${#CARGO_RUNNER_PREFIX[@]} -gt 0 ]]; then
        if [[ -f "$PROJECT_ROOT/$harness_test_log" ]]; then
            mv "$PROJECT_ROOT/$harness_test_log" "$test_log"
        else
            echo "[fault-injection] RCH did not retrieve $harness_test_log for case '$case_id'" >&2
            diagnostics_exit=69
        fi
        if [[ -f "$PROJECT_ROOT/$harness_artifact_index" ]]; then
            mv "$PROJECT_ROOT/$harness_artifact_index" "$artifact_index"
        else
            echo "[fault-injection] RCH did not retrieve $harness_artifact_index for case '$case_id'" >&2
            diagnostics_exit=69
        fi
        if [[ "$exit_code" -eq 0 && "$diagnostics_exit" -ne 0 ]]; then
            exit_code="$diagnostics_exit"
        fi
    fi

    end_epoch=$(date +%s%N 2>/dev/null || date +%s)
    if [[ ${#start_epoch} -gt 12 ]]; then
        duration_ms=$(((end_epoch - start_epoch) / 1000000))
    else
        duration_ms=$(((end_epoch - start_epoch) * 1000))
    fi

    write_case_result \
        "$result_file" \
        "$case_id" \
        "$test_name" \
        "$exit_code" \
        "$duration_ms" \
        "$log_file" \
        "$test_log" \
        "$artifact_index" \
        "$feature_name"

    if [[ "$exit_code" -eq 0 ]]; then
        echo "[fault-injection] Case '$case_id' passed (${duration_ms}ms)"
    else
        echo "[fault-injection] Case '$case_id' failed with exit code $exit_code (${duration_ms}ms)" >&2
        echo "[triage] Logs: $log_file" >&2
        echo "[triage] JSONL: $case_dir/test-log.jsonl" >&2
        echo "[triage] Artifact index: $case_dir/artifact-index.jsonl" >&2
    fi

    return "$exit_code"
}

configure_cargo_runner

assert_free_mb "$PROJECT_ROOT" "$MIN_REPO_FREE_MB" "project_root"
assert_free_mb "$ARTIFACT_DIR" "$MIN_REPO_FREE_MB" "artifact_dir"
assert_free_mb "$CARGO_TARGET_DIR" "$MIN_TMP_FREE_MB" "cargo_target_dir"
assert_free_mb "$TMPDIR" "$MIN_TMP_FREE_MB" "tmpdir"

echo "[fault-injection] CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
echo "[fault-injection] TMPDIR=$TMPDIR"

if [[ ${#CARGO_RUNNER_PREFIX[@]} -eq 0 ]]; then
    echo "[fault-injection] Cargo runner: local cargo"
else
    echo "[fault-injection] Cargo runner: env RCH_FORCE_REMOTE=$PERSISTENCE_RCH_FORCE_REMOTE RCH_REQUIRE_REMOTE=$PERSISTENCE_RCH_REQUIRE_REMOTE ${CARGO_RUNNER_PREFIX[*]} cargo"
fi

jsonl_exit=0
sqlite_exit=0
summary_exit=0

run_case "jsonl" "jsonl_fault_injection_flush_windows_preserve_integrity" || jsonl_exit=$?
run_case "sqlite" "sqlite_fault_injection_flush_windows_preserve_integrity" "sqlite-sessions" || sqlite_exit=$?

set +e
python3 - "$ARTIFACT_DIR" "$CORRELATION_ID" "$STAMP" <<'PY'
import json
import sys
from pathlib import Path

artifact_dir = Path(sys.argv[1])
correlation_id = sys.argv[2]
timestamp = sys.argv[3]


def load_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def load_jsonl(path: Path) -> list[dict]:
    records: list[dict] = []
    if not path.exists():
        return records
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = raw.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            records.append(value)
    return records


def case_checks(
    case_id: str,
    expected_test_name: str,
    expected_fault_message: str,
    expected_summary_artifact: str,
) -> dict:
    case_dir = artifact_dir / case_id
    result = load_json(case_dir / "result.json")
    logs = load_jsonl(case_dir / "test-log.jsonl")
    artifacts = load_jsonl(case_dir / "artifact-index.jsonl")

    has_fault_log = any(
        record.get("category") == "fault"
        and expected_fault_message in str(record.get("message", ""))
        for record in logs
    )
    has_summary_artifact = any(
        record.get("name") == expected_summary_artifact for record in artifacts
    )
    has_current_correlation = bool(logs) and all(
        record.get("ci_correlation_id") == correlation_id for record in logs
    )
    has_expected_test_identity = bool(artifacts) and all(
        record.get("test") == expected_test_name for record in artifacts
    )

    checks = {
        "test_command_passed": result.get("exit_code") == 0,
        "fault_log_emitted": has_fault_log,
        "summary_artifact_indexed": has_summary_artifact,
        "correlation_id_current": has_current_correlation,
        "test_identity_current": has_expected_test_identity,
    }

    return {
        "case_id": case_id,
        "result_file": str(case_dir / "result.json"),
        "checks": checks,
        "test_log_records": len(logs),
        "artifact_records": len(artifacts),
        "passed": all(checks.values()),
    }


jsonl_case = case_checks(
    "jsonl",
    "e2e_jsonl_fault_injection_flush_windows",
    "jsonl mid-flush failure",
    "jsonl-fault-window-summary.json",
)
sqlite_case = case_checks(
    "sqlite",
    "e2e_sqlite_fault_injection_flush_windows",
    "sqlite mid-flush failure",
    "sqlite-fault-window-summary.json",
)

overall_passed = jsonl_case["passed"] and sqlite_case["passed"]
summary = {
    "schema": "pi.e2e.persistence_fault_injection.summary.v1",
    "correlation_id": correlation_id,
    "timestamp": timestamp,
    "assertions": {
        "crash_windows": ["pre_flush", "mid_flush", "post_flush"],
        "integrity_invariants": [
            "no_duplication",
            "no_data_loss",
            "ordering_preserved",
        ],
    },
    "cases": [jsonl_case, sqlite_case],
    "overall_passed": overall_passed,
}

summary_path = artifact_dir / "integrity-summary.json"
summary_path.write_text(json.dumps(summary, indent=2), encoding="utf-8")
print(f"[fault-injection] Integrity summary: {summary_path}")

sys.exit(0 if overall_passed else 1)
PY
summary_exit=$?
set -e

overall_exit=0
if [[ "$jsonl_exit" -ne 0 || "$sqlite_exit" -ne 0 || "$summary_exit" -ne 0 ]]; then
    overall_exit=1
fi

cat >"$ARTIFACT_DIR/run-manifest.json" <<EOF
{
  "schema": "pi.e2e.persistence_fault_injection.manifest.v1",
  "correlation_id": "$CORRELATION_ID",
  "timestamp": "$STAMP",
  "artifact_dir": "$ARTIFACT_DIR",
  "runner_mode": "$CARGO_RUNNER_MODE",
  "rch_require_remote": $PERSISTENCE_RCH_REQUIRE_REMOTE,
  "result_files": [
    "$ARTIFACT_DIR/jsonl/result.json",
    "$ARTIFACT_DIR/sqlite/result.json",
    "$ARTIFACT_DIR/integrity-summary.json"
  ],
  "exit_codes": {
    "jsonl": $jsonl_exit,
    "sqlite": $sqlite_exit,
    "summary_validation": $summary_exit,
    "overall": $overall_exit
  }
}
EOF

echo "[fault-injection] Completed with exit code $overall_exit"
echo "[fault-injection] Artifacts: $ARTIFACT_DIR"

exit "$overall_exit"
