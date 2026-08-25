#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

# RCH retrieves diagnostics through fixed project-root report names. Serialize
# this runner across agents before creating any run-specific state so two
# invocations cannot race between the pre-existing-file check, remote execution,
# and report move.
if [[ "${PERSISTENCE_REPORT_LOCK_HELD:-0}" != "1" ]]; then
    exec python3 - "$0" "$@" <<'PY'
import fcntl
import os
import subprocess
import sys
from pathlib import Path

script = Path(sys.argv[1]).resolve()
arguments = sys.argv[2:]
lock_path = Path(
    os.environ.get(
        "PERSISTENCE_REPORT_LOCK_PATH",
        "/tmp/pi_agent_rust-persistence-fault-injection-reports.lock",
    )
)
lock_path.parent.mkdir(parents=True, exist_ok=True)
with lock_path.open("a+", encoding="utf-8") as lock_file:
    fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
    child_env = os.environ.copy()
    child_env["PERSISTENCE_REPORT_LOCK_HELD"] = "1"
    completed = subprocess.run(["bash", str(script), *arguments], env=child_env)
    raise SystemExit(completed.returncode)
PY
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_NONCE="$(python3 -c 'import secrets; print(secrets.token_hex(6))')"
RUN_ID="$STAMP-$RUN_NONCE"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/persistence-fault-injection/$RUN_ID}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-persistence-fault-injection-$RUN_ID}"
export CI_CORRELATION_ID="$CORRELATION_ID"
export RUST_LOG="${RUST_LOG:-info}"
SOURCE_COMMIT="$(git rev-parse HEAD 2>/dev/null || echo unknown)"

source_dirty_state() {
    if [[ -n "$(git status --porcelain=v1 --untracked-files=all 2>/dev/null)" ]]; then
        printf '%s\n' true
    else
        printf '%s\n' false
    fi
}

SOURCE_DIRTY="$(source_dirty_state)"

source_tree_digest() {
    python3 - "$PROJECT_ROOT" <<'PY'
import hashlib
import os
import stat
import subprocess
import sys
from pathlib import Path

root = Path(sys.argv[1])
listed = subprocess.run(
    ["git", "-C", str(root), "ls-files", "-c", "-o", "--exclude-standard", "-z"],
    check=True,
    stdout=subprocess.PIPE,
).stdout
digest = hashlib.sha256()
for raw_relative in sorted(filter(None, listed.split(b"\0"))):
    relative = os.fsdecode(raw_relative)
    path = root / relative
    digest.update(b"path\0" + raw_relative + b"\0")
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        digest.update(b"missing\0")
        continue
    digest.update(f"mode:{stat.S_IMODE(metadata.st_mode):o}\0".encode())
    if stat.S_ISLNK(metadata.st_mode):
        digest.update(b"symlink\0" + os.fsencode(os.readlink(path)) + b"\0")
    elif stat.S_ISREG(metadata.st_mode):
        digest.update(b"file\0")
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        digest.update(b"\0")
    else:
        digest.update(f"other:{stat.S_IFMT(metadata.st_mode):o}\0".encode())
print(digest.hexdigest())
PY
}

SOURCE_TREE_DIGEST="$(source_tree_digest)"

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

epoch_ms() {
    python3 -c 'import time; print(time.monotonic_ns() // 1_000_000)'
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
  "run_id": "$CORRELATION_ID",
  "correlation_id": "$CORRELATION_ID",
  "source_commit": "$SOURCE_COMMIT",
  "source_dirty": $SOURCE_DIRTY,
  "source_tree_sha256": "$SOURCE_TREE_DIGEST",
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
    local source_commit source_dirty source_digest

    mkdir -p "$case_dir"
    source_commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
    source_dirty="$(source_dirty_state)"
    source_digest="$(source_tree_digest)"
    if [[ "$source_commit" != "$SOURCE_COMMIT" || "$source_dirty" != "$SOURCE_DIRTY" || "$source_digest" != "$SOURCE_TREE_DIGEST" ]]; then
        echo "[fault-injection] Source tree drifted before case '$case_id'" >&2
        write_case_result \
            "$result_file" \
            "$case_id" \
            "$test_name" \
            70 \
            0 \
            "$log_file" \
            "$test_log" \
            "$artifact_index" \
            "$feature_name"
        return 70
    fi
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
    start_epoch=$(epoch_ms)

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

    end_epoch=$(epoch_ms)
    duration_ms=$((end_epoch - start_epoch))
    source_commit="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
    source_dirty="$(source_dirty_state)"
    source_digest="$(source_tree_digest)"
    if [[ "$source_commit" != "$SOURCE_COMMIT" || "$source_dirty" != "$SOURCE_DIRTY" || "$source_digest" != "$SOURCE_TREE_DIGEST" ]]; then
        echo "[fault-injection] Source tree drifted while case '$case_id' ran" >&2
        exit_code=70
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

run_case "jsonl" "jsonl_fault_injection_flush_windows_preserve_integrity" "internal-persistence-fault-injection" || jsonl_exit=$?
run_case "sqlite" "sqlite_fault_injection_flush_windows_preserve_integrity" "sqlite-sessions,internal-persistence-fault-injection" || sqlite_exit=$?

set +e
SOURCE_COMMIT_FINAL="$(git rev-parse HEAD 2>/dev/null || echo unknown)"
SOURCE_DIRTY_FINAL="$(source_dirty_state)"
SOURCE_TREE_DIGEST_FINAL="$(source_tree_digest)"
python3 - "$ARTIFACT_DIR" "$CORRELATION_ID" "$STAMP" "$SOURCE_COMMIT" "$SOURCE_DIRTY" "$SOURCE_TREE_DIGEST" "$SOURCE_COMMIT_FINAL" "$SOURCE_DIRTY_FINAL" "$SOURCE_TREE_DIGEST_FINAL" <<'PY'
import base64
import binascii
import hashlib
import json
import re
import sys
from datetime import datetime
from pathlib import Path

artifact_dir = Path(sys.argv[1])
correlation_id = sys.argv[2]
timestamp = sys.argv[3]
source_commit = sys.argv[4]
source_dirty = sys.argv[5] == "true"
source_tree_digest = sys.argv[6]
source_commit_final = sys.argv[7]
source_dirty_final = sys.argv[8] == "true"
source_tree_digest_final = sys.argv[9]


def load_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def load_jsonl(path: Path) -> list[dict]:
    records: list[dict] = []
    if not path.exists():
        return records
    for line_number, raw in enumerate(
        path.read_text(encoding="utf-8", errors="strict").splitlines(), start=1
    ):
        line = raw.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise ValueError(f"{path}:{line_number}: invalid JSON: {error}") from error
        if not isinstance(value, dict):
            raise ValueError(f"{path}:{line_number}: expected a JSON object")
        records.append(value)
    return records


def timestamp_is_valid(value: object) -> bool:
    if not isinstance(value, str) or not value.strip():
        return False
    normalized = value.strip()
    if normalized.endswith("Z"):
        normalized = f"{normalized[:-1]}+00:00"
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return False
    return parsed.tzinfo is not None


def log_record_is_valid(record: dict) -> bool:
    required_fields = {
        "schema",
        "type",
        "trace_id",
        "seq",
        "ts",
        "t_ms",
        "level",
        "category",
        "message",
    }
    if not required_fields.issubset(record):
        return False
    if record.get("schema") != "pi.test.log.v2" or record.get("type") != "log":
        return False
    if not isinstance(record.get("trace_id"), str) or not record["trace_id"].strip():
        return False
    seq = record.get("seq")
    elapsed_ms = record.get("t_ms")
    if isinstance(seq, bool) or not isinstance(seq, int) or seq < 1:
        return False
    if isinstance(elapsed_ms, bool) or not isinstance(elapsed_ms, int) or elapsed_ms < 0:
        return False
    if not timestamp_is_valid(record.get("ts")):
        return False
    if record.get("level") not in {"debug", "info", "warn", "error"}:
        return False
    return all(
        isinstance(record.get(field), str)
        for field in ("category", "message")
    )


def artifact_envelope_is_valid(record: dict, expected_test_name: str) -> bool:
    required_fields = {"schema", "type", "seq", "ts", "t_ms", "name", "path"}
    if not required_fields.issubset(record):
        return False
    if record.get("schema") != "pi.test.artifact.v1":
        return False
    if record.get("type") != "artifact" or record.get("test") != expected_test_name:
        return False
    seq = record.get("seq")
    elapsed_ms = record.get("t_ms")
    if isinstance(seq, bool) or not isinstance(seq, int) or seq < 1:
        return False
    if isinstance(elapsed_ms, bool) or not isinstance(elapsed_ms, int) or elapsed_ms < 0:
        return False
    if not timestamp_is_valid(record.get("ts")):
        return False
    return all(
        isinstance(record.get(field), str) and bool(record[field].strip())
        for field in ("name", "path")
    )


def artifact_record_is_valid(
    record: dict,
    expected_test_name: str,
    expected_summary_artifact: str,
) -> bool:
    if not artifact_envelope_is_valid(record, expected_test_name):
        return False
    if record.get("name") != expected_summary_artifact:
        return False
    raw_path = record.get("path")
    if not isinstance(raw_path, str) or not raw_path.strip():
        return False
    if Path(raw_path).name != expected_summary_artifact:
        return False
    size_bytes = record.get("size_bytes")
    if isinstance(size_bytes, bool) or not isinstance(size_bytes, int) or size_bytes <= 0:
        return False
    sha256 = record.get("sha256")
    return isinstance(sha256, str) and re.fullmatch(r"[0-9a-f]{64}", sha256) is not None


def inline_summary_bytes_are_valid(
    diagnostic_records: list[dict],
    artifact_record: dict,
    case_dir: Path,
    case_id: str,
    expected_summary_artifact: str,
) -> bool:
    payload_records = [
        record
        for record in diagnostic_records
        if record.get("schema") == "pi.test.log.v2"
        and record.get("type") == "log"
        and record.get("category") == "artifact_payload"
        and isinstance(record.get("context"), dict)
        and record["context"].get("artifact_name") == expected_summary_artifact
    ]
    if len(payload_records) != 1:
        return False
    context = payload_records[0]["context"]
    if context.get("content_encoding") != "base64":
        return False
    encoded = context.get("content_base64")
    if not isinstance(encoded, str) or not encoded:
        return False
    try:
        payload = base64.b64decode(encoded, validate=True)
    except (ValueError, binascii.Error):
        return False
    digest = hashlib.sha256(payload).hexdigest()
    if digest != context.get("content_sha256") or digest != artifact_record.get("sha256"):
        return False
    if len(payload) != artifact_record.get("size_bytes"):
        return False
    try:
        summary = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return False
    base_message = f"{case_id}-base"
    mid_message = f"{case_id}-midflush-pending"
    post_message = f"{case_id}-postflush-persisted"
    if case_id == "jsonl":
        expected_mid_flush = [base_message, mid_message]
        expected_post_flush = [base_message, mid_message, post_message]
    else:
        expected_mid_flush = [base_message]
        expected_post_flush = [base_message, post_message]
    if summary != {
        "scenario": f"{case_id}_fault_windows",
        "windows": {
            "pre_flush": [base_message],
            "mid_flush": expected_mid_flush,
            "post_flush": expected_post_flush,
        },
    }:
        return False
    local_summary_path = case_dir / expected_summary_artifact
    local_summary_path.write_bytes(payload)
    return local_summary_path.is_file()


def case_checks(
    case_id: str,
    expected_test_name: str,
    expected_fault_message: str,
    expected_summary_artifact: str,
) -> dict:
    case_dir = artifact_dir / case_id
    result = load_json(case_dir / "result.json")
    diagnostic_records = load_jsonl(case_dir / "test-log.jsonl")
    logs = [
        record
        for record in diagnostic_records
        if record.get("schema") == "pi.test.log.v2" and record.get("type") == "log"
    ]
    artifacts = load_jsonl(case_dir / "artifact-index.jsonl")

    has_fault_log = any(
        record.get("category") == "fault"
        and expected_fault_message in str(record.get("message", ""))
        for record in logs
    )
    summary_artifacts = [
        record
        for record in artifacts
        if record.get("name") == expected_summary_artifact
    ]
    has_summary_artifact = len(summary_artifacts) == 1
    has_valid_summary_artifact = has_summary_artifact and artifact_record_is_valid(
        summary_artifacts[0], expected_test_name, expected_summary_artifact
    )
    has_verified_summary_bytes = has_valid_summary_artifact and inline_summary_bytes_are_valid(
        diagnostic_records,
        summary_artifacts[0],
        case_dir,
        case_id,
        expected_summary_artifact,
    )
    has_current_correlation = bool(logs) and all(
        record.get("ci_correlation_id") == correlation_id for record in logs
    )
    diagnostic_log_schema_valid = bool(logs) and all(
        log_record_is_valid(record)
        if record.get("schema") == "pi.test.log.v2"
        else artifact_envelope_is_valid(record, expected_test_name)
        for record in diagnostic_records
    )
    has_expected_test_identity = bool(artifacts) and all(
        record.get("test") == expected_test_name for record in artifacts
    )
    artifact_index_schema_valid = bool(artifacts) and all(
        artifact_envelope_is_valid(record, expected_test_name) for record in artifacts
    )

    checks = {
        "test_command_passed": result.get("exit_code") == 0,
        "result_identity_current": (
            result.get("run_id") == correlation_id
            and result.get("correlation_id") == correlation_id
            and result.get("source_commit") == source_commit
            and result.get("source_dirty") == source_dirty
            and result.get("source_tree_sha256") == source_tree_digest
        ),
        "fault_log_emitted": has_fault_log,
        "summary_artifact_indexed": has_summary_artifact,
        "summary_artifact_schema_valid": has_valid_summary_artifact,
        "summary_artifact_bytes_verified": has_verified_summary_bytes,
        "diagnostic_log_schema_valid": diagnostic_log_schema_valid,
        "artifact_index_schema_valid": artifact_index_schema_valid,
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

source_tree_stable = (
    source_commit_final == source_commit
    and source_dirty_final == source_dirty
    and source_tree_digest_final == source_tree_digest
)
overall_passed = jsonl_case["passed"] and sqlite_case["passed"] and source_tree_stable
summary = {
    "schema": "pi.e2e.persistence_fault_injection.summary.v1",
    "run_id": correlation_id,
    "correlation_id": correlation_id,
    "source_commit": source_commit,
    "source_dirty": source_dirty,
    "source_tree_sha256": source_tree_digest,
    "source_commit_final": source_commit_final,
    "source_dirty_final": source_dirty_final,
    "source_tree_sha256_final": source_tree_digest_final,
    "source_tree_stable": source_tree_stable,
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
  "run_id": "$CORRELATION_ID",
  "correlation_id": "$CORRELATION_ID",
  "source_commit": "$SOURCE_COMMIT",
  "source_dirty": $SOURCE_DIRTY,
  "source_tree_sha256": "$SOURCE_TREE_DIGEST",
  "source_commit_final": "$SOURCE_COMMIT_FINAL",
  "source_dirty_final": $SOURCE_DIRTY_FINAL,
  "source_tree_sha256_final": "$SOURCE_TREE_DIGEST_FINAL",
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
