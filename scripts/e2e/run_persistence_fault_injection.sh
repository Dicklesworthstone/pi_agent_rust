#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

# RCH retrieves diagnostics through fixed project-root report names. Serialize
# this runner across agents before creating any run-specific state so two
# invocations cannot race between the pre-existing-file check, remote execution,
# and report move. A caller-provided "lock held" flag is not authority: the
# child must also inherit an open descriptor for the one fixed lock inode.
if python3 - "${PERSISTENCE_REPORT_LOCK_HELD:-0}" "${_PI_PERSISTENCE_REPORT_LOCK_FD:-}" <<'PY'
import fcntl
import os
import stat
import sys
from pathlib import Path

lock_path = Path("/tmp/pi_agent_rust-persistence-fault-injection-reports.lock")
try:
    if sys.argv[1] != "1":
        raise ValueError("lock-held flag is absent")
    lock_fd = int(sys.argv[2])
    descriptor = os.fstat(lock_fd)
    path_metadata = lock_path.lstat()
    if not stat.S_ISREG(descriptor.st_mode) or not stat.S_ISREG(path_metadata.st_mode):
        raise ValueError("report lock is not a regular file")
    if (descriptor.st_dev, descriptor.st_ino) != (path_metadata.st_dev, path_metadata.st_ino):
        raise ValueError("report lock descriptor does not match the fixed lock path")
    fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
except (BlockingIOError, OSError, TypeError, ValueError):
    raise SystemExit(1)
PY
then
    :
else
    exec python3 - "$0" "$@" <<'PY'
import fcntl
import os
import stat
import subprocess
import sys
from pathlib import Path

script = Path(sys.argv[1]).resolve()
arguments = sys.argv[2:]
lock_path = Path("/tmp/pi_agent_rust-persistence-fault-injection-reports.lock")
lock_path.parent.mkdir(parents=True, exist_ok=True)
lock_flags = os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0)
lock_fd = os.open(lock_path, lock_flags, 0o600)
with os.fdopen(lock_fd, "a+", encoding="utf-8") as lock_file:
    lock_metadata = os.fstat(lock_file.fileno())
    if not stat.S_ISREG(lock_metadata.st_mode):
        raise SystemExit("persistence report lock is not a regular file")
    os.fchmod(lock_file.fileno(), 0o600)
    fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
    child_env = os.environ.copy()
    child_env["PERSISTENCE_REPORT_LOCK_HELD"] = "1"
    child_env["_PI_PERSISTENCE_REPORT_LOCK_FD"] = str(lock_file.fileno())
    completed = subprocess.run(
        ["bash", str(script), *arguments],
        env=child_env,
        pass_fds=(lock_file.fileno(),),
    )
    raise SystemExit(completed.returncode)
PY
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_STARTED_AT="$(python3 -c 'from datetime import datetime, timezone; print(datetime.now(timezone.utc).isoformat())')"
RUN_NONCE="$(python3 -c 'import secrets; print(secrets.token_hex(6))')"
RUN_ID="$STAMP-$RUN_NONCE"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/persistence-fault-injection/$RUN_ID}"
if [[ -L "$ARTIFACT_DIR" ]]; then
    echo "[fault-injection] Refusing symlink artifact directory: $ARTIFACT_DIR" >&2
    exit 68
fi
mkdir -p "$ARTIFACT_DIR"
if [[ ! -d "$ARTIFACT_DIR" || -L "$ARTIFACT_DIR" ]]; then
    echo "[fault-injection] Artifact path is not a real directory: $ARTIFACT_DIR" >&2
    exit 68
fi
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd -P)"
for aggregate_path in \
    "$ARTIFACT_DIR/integrity-summary.json" \
    "$ARTIFACT_DIR/.integrity-summary.pending.json" \
    "$ARTIFACT_DIR/.run-manifest.pending.json" \
    "$ARTIFACT_DIR/run-manifest.json"
do
    if [[ -e "$aggregate_path" || -L "$aggregate_path" ]]; then
        echo "[fault-injection] Refusing pre-existing aggregate artifact: $aggregate_path" >&2
        exit 68
    fi
done

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
        target = os.readlink(path)
        final_metadata = path.lstat()
        if (metadata.st_dev, metadata.st_ino, metadata.st_mtime_ns) != (
            final_metadata.st_dev,
            final_metadata.st_ino,
            final_metadata.st_mtime_ns,
        ):
            raise RuntimeError(f"source symlink changed while hashing: {relative}")
        digest.update(b"symlink\0" + os.fsencode(target) + b"\0")
    elif stat.S_ISREG(metadata.st_mode):
        digest.update(b"file\0")
        descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        with os.fdopen(descriptor, "rb") as source:
            initial_descriptor_metadata = os.fstat(source.fileno())
            if not stat.S_ISREG(initial_descriptor_metadata.st_mode):
                raise RuntimeError(f"source path stopped being a regular file: {relative}")
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
            final_descriptor_metadata = os.fstat(source.fileno())
        final_path_metadata = path.lstat()
        identity_fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns")
        initial_identity = tuple(
            getattr(initial_descriptor_metadata, field) for field in identity_fields
        )
        if (
            initial_identity
            != tuple(getattr(metadata, field) for field in identity_fields)
            or initial_identity
            != tuple(getattr(final_descriptor_metadata, field) for field in identity_fields)
            or initial_identity
            != tuple(getattr(final_path_metadata, field) for field in identity_fields)
        ):
            raise RuntimeError(f"source file changed while hashing: {relative}")
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
case "$PERSISTENCE_RCH_FORCE_REMOTE" in
    true|1) PERSISTENCE_RCH_FORCE_REMOTE=true ;;
    false|0) PERSISTENCE_RCH_FORCE_REMOTE=false ;;
    *)
        echo "PERSISTENCE_RCH_FORCE_REMOTE must be true, false, 1, or 0." >&2
        exit 64
        ;;
esac
case "$PERSISTENCE_RCH_REQUIRE_REMOTE" in
    true|1) PERSISTENCE_RCH_REQUIRE_REMOTE=true ;;
    false|0) PERSISTENCE_RCH_REQUIRE_REMOTE=false ;;
    *)
        echo "PERSISTENCE_RCH_REQUIRE_REMOTE must be true, false, 1, or 0." >&2
        exit 64
        ;;
esac
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

utc_timestamp() {
    python3 -c 'from datetime import datetime, timezone; print(datetime.now(timezone.utc).isoformat())'
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
    local completed_at
    completed_at="$(utc_timestamp)"

    python3 - \
        "$result_file" \
        "$CORRELATION_ID" \
        "$RUN_ID" \
        "$SOURCE_COMMIT" \
        "$SOURCE_DIRTY" \
        "$SOURCE_TREE_DIGEST" \
        "$case_id" \
        "$test_name" \
        "$feature_name" \
        "$exit_code" \
        "$duration_ms" \
        "$log_file" \
        "$test_log" \
        "$artifact_index" \
        "$completed_at" <<'PY'
import json
import sys
from pathlib import Path

payload = {
    "schema": "pi.e2e.persistence_fault_case.v1",
    "run_id": sys.argv[2],
    "attempt_id": sys.argv[3],
    "correlation_id": sys.argv[2],
    "source_commit": sys.argv[4],
    "source_dirty": sys.argv[5] == "true",
    "source_tree_sha256": sys.argv[6],
    "case_id": sys.argv[7],
    "suite": "e2e_session_persistence",
    "test_name": sys.argv[8],
    "feature": sys.argv[9],
    "exit_code": int(sys.argv[10]),
    "duration_ms": int(sys.argv[11]),
    "log_file": sys.argv[12],
    "test_log_jsonl": sys.argv[13],
    "artifact_index_jsonl": sys.argv[14],
    "timestamp": sys.argv[15],
}
with Path(sys.argv[1]).open("x", encoding="utf-8") as result:
    json.dump(payload, result, indent=2)
    result.write("\n")
PY
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
    local start_epoch end_epoch duration_ms exit_code diagnostics_exit tee_exit
    local -a pipeline_status
    local source_commit source_dirty source_digest

    if [[ -e "$case_dir" || -L "$case_dir" ]]; then
        echo "[fault-injection] Refusing pre-existing case artifact directory: $case_dir" >&2
        return 68
    fi
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
        if [[ -e "$PROJECT_ROOT/$harness_test_log" || -L "$PROJECT_ROOT/$harness_test_log" || -e "$PROJECT_ROOT/$harness_artifact_index" || -L "$PROJECT_ROOT/$harness_artifact_index" ]]; then
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
            --exact \
            --test-threads=1 \
            2>&1 | tee "$log_file"
    else
        run_cargo test \
            --test e2e_session_persistence \
            "$test_name" \
            -- \
            --nocapture \
            --exact \
            --test-threads=1 \
            2>&1 | tee "$log_file"
    fi
    pipeline_status=("${PIPESTATUS[@]}")
    exit_code="${pipeline_status[0]}"
    tee_exit="${pipeline_status[1]}"
    if [[ "$exit_code" -eq 0 && "$tee_exit" -ne 0 ]]; then
        echo "[fault-injection] Failed to retain output log for case '$case_id' (tee exit $tee_exit)" >&2
        exit_code=74
    fi
    set -e

    diagnostics_exit=0
    if [[ ${#CARGO_RUNNER_PREFIX[@]} -gt 0 ]]; then
        if [[ -f "$PROJECT_ROOT/$harness_test_log" && ! -L "$PROJECT_ROOT/$harness_test_log" ]]; then
            mv "$PROJECT_ROOT/$harness_test_log" "$test_log"
        else
            echo "[fault-injection] RCH did not retrieve $harness_test_log for case '$case_id'" >&2
            diagnostics_exit=69
        fi
        if [[ -f "$PROJECT_ROOT/$harness_artifact_index" && ! -L "$PROJECT_ROOT/$harness_artifact_index" ]]; then
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
RUN_FINISHED_AT="$(utc_timestamp)"
python3 - \
    "$ARTIFACT_DIR" \
    "$CORRELATION_ID" \
    "$RUN_ID" \
    "$RUN_STARTED_AT" \
    "$RUN_FINISHED_AT" \
    "$SOURCE_COMMIT" \
    "$SOURCE_DIRTY" \
    "$SOURCE_TREE_DIGEST" \
    "$SOURCE_COMMIT_FINAL" \
    "$SOURCE_DIRTY_FINAL" \
    "$SOURCE_TREE_DIGEST_FINAL" \
    "$CARGO_RUNNER_MODE" \
    "$PERSISTENCE_RCH_FORCE_REMOTE" \
    "$PERSISTENCE_RCH_REQUIRE_REMOTE" <<'PY'
import base64
import binascii
import hashlib
import json
import os
import re
import stat
import sys
from datetime import datetime, timedelta
from pathlib import Path

artifact_dir = Path(sys.argv[1])
correlation_id = sys.argv[2]
attempt_id = sys.argv[3]
run_started_at_raw = sys.argv[4]
run_finished_at_raw = sys.argv[5]
source_commit = sys.argv[6]
source_dirty = sys.argv[7] == "true"
source_tree_digest = sys.argv[8]
source_commit_final = sys.argv[9]
source_dirty_final = sys.argv[10] == "true"
source_tree_digest_final = sys.argv[11]
runner_mode = sys.argv[12]
rch_force_remote = sys.argv[13] == "true"
rch_require_remote = sys.argv[14] == "true"


def read_stable_regular(path: Path) -> bytes:
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "rb") as source:
        initial_metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(initial_metadata.st_mode):
            raise ValueError(f"{path}: expected a regular file")
        contents = source.read()
        final_descriptor_metadata = os.fstat(source.fileno())
    final_path_metadata = path.lstat()
    identity_fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns")
    initial_identity = tuple(getattr(initial_metadata, field) for field in identity_fields)
    if (
        not stat.S_ISREG(final_path_metadata.st_mode)
        or initial_identity
        != tuple(getattr(final_descriptor_metadata, field) for field in identity_fields)
        or initial_identity
        != tuple(getattr(final_path_metadata, field) for field in identity_fields)
        or len(contents) != initial_metadata.st_size
    ):
        raise ValueError(f"{path}: file changed while read")
    return contents


def load_json(path: Path) -> dict:
    value = json.loads(read_stable_regular(path))
    if not isinstance(value, dict):
        raise ValueError(f"{path}: expected a JSON object")
    return value


def load_jsonl(path: Path) -> list[dict]:
    records: list[dict] = []
    if not path.exists():
        return records
    for line_number, raw in enumerate(
        read_stable_regular(path).decode("utf-8", errors="strict").splitlines(), start=1
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


def parse_timestamp(value: object):
    if not isinstance(value, str) or not value.strip():
        return None
    normalized = value.strip()
    if normalized.endswith("Z"):
        normalized = f"{normalized[:-1]}+00:00"
    try:
        parsed = datetime.fromisoformat(normalized)
    except ValueError:
        return None
    return parsed if parsed.tzinfo is not None else None


run_started_at = parse_timestamp(run_started_at_raw)
run_finished_at = parse_timestamp(run_finished_at_raw)
if run_started_at is None or run_finished_at is None or run_finished_at < run_started_at:
    raise ValueError("runner timestamps are invalid or reversed")
timestamp_skew = timedelta(seconds=120)


def timestamp_is_current(value: object) -> bool:
    parsed = parse_timestamp(value)
    return (
        parsed is not None
        and parsed >= run_started_at - timestamp_skew
        and parsed <= run_finished_at + timestamp_skew
    )


def log_record_is_valid(record: dict, expected_test_name: str) -> bool:
    required_fields = {
        "schema",
        "type",
        "test",
        "trace_id",
        "ci_correlation_id",
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
    if record.get("test") != expected_test_name:
        return False
    if not isinstance(record.get("trace_id"), str) or not record["trace_id"].strip():
        return False
    if (
        not isinstance(record.get("ci_correlation_id"), str)
        or not record["ci_correlation_id"].strip()
    ):
        return False
    seq = record.get("seq")
    elapsed_ms = record.get("t_ms")
    if isinstance(seq, bool) or not isinstance(seq, int) or seq < 1:
        return False
    if isinstance(elapsed_ms, bool) or not isinstance(elapsed_ms, int) or elapsed_ms < 0:
        return False
    if not timestamp_is_current(record.get("ts")):
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
    if not timestamp_is_current(record.get("ts")):
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
    expected_test_name: str,
    expected_summary_artifact: str,
) -> bool:
    payload_records = [
        record
        for record in diagnostic_records
        if record.get("schema") == "pi.test.log.v2"
        and record.get("type") == "log"
        and record.get("test") == expected_test_name
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
        "schema": "pi.e2e.persistence_fault_case_summary.v1",
        "case_id": case_id,
        "test_name": expected_test_name,
        "correlation_id": correlation_id,
        "scenario": f"{case_id}_fault_windows",
        "windows": {
            "pre_flush": [base_message],
            "mid_flush": expected_mid_flush,
            "post_flush": expected_post_flush,
        },
    }:
        return False
    case_root = case_dir.resolve()
    local_summary_path = (case_dir / expected_summary_artifact).resolve()
    try:
        relative_summary_path = local_summary_path.relative_to(case_root)
    except ValueError:
        return False
    if relative_summary_path.parts != (expected_summary_artifact,):
        return False
    with local_summary_path.open("xb") as local_summary:
        local_summary.write(payload)
    if (
        not local_summary_path.is_file()
        or local_summary_path.is_symlink()
        or hashlib.sha256(read_stable_regular(local_summary_path)).hexdigest() != digest
    ):
        return False
    artifact_record["remote_path"] = artifact_record["path"]
    artifact_record["path"] = str(local_summary_path)
    return True


def canonical_summary_artifact_is_valid(
    artifact_record: dict,
    case_dir: Path,
    expected_summary_artifact: str,
) -> bool:
    raw_path = artifact_record.get("path")
    if not isinstance(raw_path, str) or not raw_path:
        return False
    try:
        case_root = case_dir.resolve(strict=True)
        summary_path = Path(raw_path).resolve(strict=True)
        relative_summary_path = summary_path.relative_to(case_root)
    except (FileNotFoundError, OSError, ValueError):
        return False
    if relative_summary_path.parts != (expected_summary_artifact,):
        return False
    try:
        payload = read_stable_regular(summary_path)
    except OSError:
        return False
    return (
        summary_path.is_file()
        and len(payload) == artifact_record.get("size_bytes")
        and hashlib.sha256(payload).hexdigest() == artifact_record.get("sha256")
    )


def case_checks(
    case_id: str,
    expected_test_name: str,
    expected_cargo_test: str,
    expected_feature: str,
    expected_fault_message: str,
    expected_summary_artifact: str,
) -> dict:
    # Two identities per case: `expected_test_name` is the harness test id the
    # Rust test stamps on its JSONL diagnostics; `expected_cargo_test` is the
    # cargo test function `run_case` executed and recorded in result.json.
    case_dir = artifact_dir / case_id
    result = load_json(case_dir / "result.json")
    diagnostic_records = load_jsonl(case_dir / "test-log.jsonl")
    logs = [
        record
        for record in diagnostic_records
        if record.get("schema") == "pi.test.log.v2" and record.get("type") == "log"
    ]
    artifacts = load_jsonl(case_dir / "artifact-index.jsonl")
    try:
        read_stable_regular(case_dir / "output.log")
        output_log_regular = True
    except (OSError, ValueError):
        output_log_regular = False

    has_fault_log = any(
        record.get("test") == expected_test_name
        and record.get("category") == "fault"
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
        expected_test_name,
        expected_summary_artifact,
    )
    has_confined_summary_artifact = (
        has_verified_summary_bytes
        and canonical_summary_artifact_is_valid(
            summary_artifacts[0], case_dir, expected_summary_artifact
        )
    )
    if has_confined_summary_artifact:
        (case_dir / "artifact-index.jsonl").write_text(
            "".join(
                f"{json.dumps(record, sort_keys=True, separators=(',', ':'))}\n"
                for record in artifacts
            ),
            encoding="utf-8",
        )
    has_current_correlation = bool(logs) and all(
        record.get("ci_correlation_id") == correlation_id for record in logs
    )
    diagnostic_log_schema_valid = bool(logs) and all(
        log_record_is_valid(record, expected_test_name)
        if record.get("schema") == "pi.test.log.v2"
        else artifact_envelope_is_valid(record, expected_test_name)
        for record in diagnostic_records
    )
    has_expected_test_identity = (
        bool(logs)
        and all(record.get("test") == expected_test_name for record in logs)
        and bool(artifacts)
        and all(record.get("test") == expected_test_name for record in artifacts)
    )
    artifact_index_schema_valid = bool(artifacts) and all(
        artifact_envelope_is_valid(record, expected_test_name) for record in artifacts
    )
    result_exit_code = result.get("exit_code")
    result_duration_ms = result.get("duration_ms")
    result_schema_valid = (
        result.get("schema") == "pi.e2e.persistence_fault_case.v1"
        and isinstance(result.get("run_id"), str)
        and isinstance(result.get("attempt_id"), str)
        and bool(result["attempt_id"].strip())
        and isinstance(result.get("correlation_id"), str)
        and isinstance(result.get("source_commit"), str)
        and isinstance(result.get("source_dirty"), bool)
        and isinstance(result.get("source_tree_sha256"), str)
        and isinstance(result.get("case_id"), str)
        and isinstance(result.get("suite"), str)
        and isinstance(result.get("test_name"), str)
        and isinstance(result.get("feature"), str)
        and not isinstance(result_exit_code, bool)
        and isinstance(result_exit_code, int)
        and not isinstance(result_duration_ms, bool)
        and isinstance(result_duration_ms, int)
        and result_duration_ms >= 0
        and result.get("log_file") == str(case_dir / "output.log")
        and result.get("test_log_jsonl") == str(case_dir / "test-log.jsonl")
        and result.get("artifact_index_jsonl") == str(case_dir / "artifact-index.jsonl")
        and timestamp_is_current(result.get("timestamp"))
    )
    diagnostic_sequences = [record.get("seq") for record in diagnostic_records]
    diagnostic_elapsed_ms = [record.get("t_ms") for record in diagnostic_records]
    diagnostic_sequence_valid = bool(diagnostic_records) and all(
        not isinstance(value, bool) and isinstance(value, int) and value >= 0
        for value in diagnostic_elapsed_ms
    ) and diagnostic_sequences == list(range(1, len(diagnostic_records) + 1)) and all(
        current <= following
        for current, following in zip(diagnostic_elapsed_ms, diagnostic_elapsed_ms[1:])
    )
    diagnostic_trace_ids = [record.get("trace_id") for record in logs]
    diagnostic_trace_bound = (
        bool(logs)
        and all(isinstance(trace_id, str) and bool(trace_id.strip()) for trace_id in diagnostic_trace_ids)
        and len(set(diagnostic_trace_ids)) == 1
        and all(record.get("ci_correlation_id") == correlation_id for record in logs)
    )

    checks = {
        "test_command_passed": result_schema_valid and result_exit_code == 0,
        "output_log_regular": output_log_regular,
        "result_schema_valid": result_schema_valid,
        "result_identity_current": (
            result.get("run_id") == correlation_id
            and result.get("attempt_id") == attempt_id
            and result.get("correlation_id") == correlation_id
            and result.get("source_commit") == source_commit
            and result.get("source_dirty") == source_dirty
            and result.get("source_tree_sha256") == source_tree_digest
            and result.get("case_id") == case_id
            and result.get("suite") == "e2e_session_persistence"
            and result.get("test_name") == expected_cargo_test
            and result.get("feature") == expected_feature
        ),
        "fault_log_emitted": has_fault_log,
        "summary_artifact_indexed": has_summary_artifact,
        "summary_artifact_schema_valid": has_valid_summary_artifact,
        "summary_artifact_bytes_verified": has_verified_summary_bytes,
        "summary_artifact_path_confined": has_confined_summary_artifact,
        "diagnostic_log_schema_valid": diagnostic_log_schema_valid,
        "artifact_index_schema_valid": artifact_index_schema_valid,
        "diagnostic_sequence_valid": diagnostic_sequence_valid,
        "diagnostic_trace_bound": diagnostic_trace_bound,
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


case_check_names = (
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
)


def guarded_case_checks(*args: str) -> dict:
    case_id = args[0]
    try:
        return case_checks(*args)
    except Exception as error:
        return {
            "case_id": case_id,
            "result_file": str(artifact_dir / case_id / "result.json"),
            "checks": {name: False for name in case_check_names},
            "test_log_records": 0,
            "artifact_records": 0,
            "evidence_error": f"{type(error).__name__}: {error}",
            "passed": False,
        }


jsonl_case = guarded_case_checks(
    "jsonl",
    "e2e_jsonl_fault_injection_flush_windows",
    "jsonl_fault_injection_flush_windows_preserve_integrity",
    "internal-persistence-fault-injection",
    "jsonl mid-flush failure",
    "jsonl-fault-window-summary.json",
)
sqlite_case = guarded_case_checks(
    "sqlite",
    "e2e_sqlite_fault_injection_flush_windows",
    "sqlite_fault_injection_flush_windows_preserve_integrity",
    "sqlite-sessions,internal-persistence-fault-injection",
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
    "attempt_id": attempt_id,
    "correlation_id": correlation_id,
    "source_commit": source_commit,
    "source_dirty": source_dirty,
    "source_tree_sha256": source_tree_digest,
    "source_commit_final": source_commit_final,
    "source_dirty_final": source_dirty_final,
    "source_tree_sha256_final": source_tree_digest_final,
    "source_tree_stable": source_tree_stable,
    "run_started_at": run_started_at_raw,
    "timestamp": run_finished_at_raw,
    "runner_mode": runner_mode,
    "rch_force_remote": rch_force_remote,
    "rch_require_remote": rch_require_remote,
    "execution_attestation": "configuration_only",
    "terminal_state": "summary_validated",
    "assertions": {
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
    },
    "cases": [jsonl_case, sqlite_case],
    "validation_passed": overall_passed,
}

summary_path = artifact_dir / ".integrity-summary.pending.json"
with summary_path.open("x", encoding="utf-8") as pending_summary:
    json.dump(summary, pending_summary, indent=2)
    pending_summary.write("\n")
    pending_summary.flush()
    os.fsync(pending_summary.fileno())

sys.exit(0 if overall_passed else 1)
PY
summary_exit=$?
set -e

# A failed integrity summary must name its reason on stdout: the DSR lane and
# the bench_schema harness only keep the runner's output, not the JSON, and a
# bare "exit code 1" after two passing cases is not a diagnosable result.
if [[ "$summary_exit" -ne 0 && -f "$ARTIFACT_DIR/.integrity-summary.pending.json" ]]; then
    python3 - "$ARTIFACT_DIR/.integrity-summary.pending.json" <<'PY' || true
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    summary = json.load(handle)
for case in summary.get("cases", []):
    checks = case.get("checks", {})
    failed = [name for name, ok in checks.items() if ok is not True]
    if failed or case.get("evidence_error"):
        print(
            f"[fault-injection] Case '{case.get('case_id')}' evidence checks failed: "
            f"{', '.join(failed) or 'n/a'}; error={case.get('evidence_error')}"
        )
if summary.get("source_tree_stable") is not True:
    print(
        "[fault-injection] Source tree moved during the run: "
        f"commit {summary.get('source_commit')} -> {summary.get('source_commit_final')}, "
        f"dirty {summary.get('source_dirty')} -> {summary.get('source_dirty_final')}, "
        f"digest {str(summary.get('source_tree_sha256'))[:12]} -> "
        f"{str(summary.get('source_tree_sha256_final'))[:12]}"
    )
PY
fi

overall_exit=0
if [[ "$jsonl_exit" -ne 0 || "$sqlite_exit" -ne 0 || "$summary_exit" -ne 0 ]]; then
    overall_exit=1
fi

python3 - \
    "$ARTIFACT_DIR/.integrity-summary.pending.json" \
    "$ARTIFACT_DIR/integrity-summary.json" \
    "$ARTIFACT_DIR" <<'PY'
import os
import stat
import sys
from pathlib import Path

pending = Path(sys.argv[1])
published = Path(sys.argv[2])
artifact_dir = Path(sys.argv[3])


def read_stable_regular(path: Path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "rb") as source:
        initial_metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(initial_metadata.st_mode):
            raise ValueError(f"publication artifact is not a regular file: {path}")
        contents = source.read()
        final_descriptor_metadata = os.fstat(source.fileno())
    final_path_metadata = path.lstat()
    identity_fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns")
    initial_identity = tuple(getattr(initial_metadata, field) for field in identity_fields)
    if (
        not stat.S_ISREG(final_descriptor_metadata.st_mode)
        or not stat.S_ISREG(final_path_metadata.st_mode)
        or initial_identity
        != tuple(getattr(final_descriptor_metadata, field) for field in identity_fields)
        or initial_identity
        != tuple(getattr(final_path_metadata, field) for field in identity_fields)
        or len(contents) != initial_metadata.st_size
    ):
        raise ValueError(f"publication artifact changed while read: {path}")
    return contents, initial_metadata


pending_bytes, pending_metadata = read_stable_regular(pending)
if published.exists() or published.is_symlink():
    raise SystemExit("integrity summary publication target is unsafe")
os.link(pending, published, follow_symlinks=False)
published_metadata = published.lstat()
if (
    not stat.S_ISREG(published_metadata.st_mode)
    or (published_metadata.st_dev, published_metadata.st_ino)
    != (pending_metadata.st_dev, pending_metadata.st_ino)
):
    raise SystemExit("published integrity summary inode does not match attested bytes")
pending.unlink()
directory_descriptor = os.open(
    artifact_dir,
    os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
)
try:
    os.fsync(directory_descriptor)
finally:
    os.close(directory_descriptor)
PY

MANIFEST_COMPLETED_AT="$(utc_timestamp)"
python3 - \
    "$ARTIFACT_DIR/.run-manifest.pending.json" \
    "$ARTIFACT_DIR/integrity-summary.json" \
    "$CORRELATION_ID" \
    "$RUN_ID" \
    "$SOURCE_COMMIT" \
    "$SOURCE_DIRTY" \
    "$SOURCE_TREE_DIGEST" \
    "$SOURCE_COMMIT_FINAL" \
    "$SOURCE_DIRTY_FINAL" \
    "$SOURCE_TREE_DIGEST_FINAL" \
    "$MANIFEST_COMPLETED_AT" \
    "$ARTIFACT_DIR" \
    "$CARGO_RUNNER_MODE" \
    "$PERSISTENCE_RCH_FORCE_REMOTE" \
    "$PERSISTENCE_RCH_REQUIRE_REMOTE" \
    "$jsonl_exit" \
    "$sqlite_exit" \
    "$summary_exit" \
    "$overall_exit" <<'PY'
import hashlib
import json
import os
import stat
import sys
from pathlib import Path

manifest_path = Path(sys.argv[1])
final_summary_path = Path(sys.argv[2])


def read_stable_regular(path: Path, *, missing_ok: bool = False):
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except FileNotFoundError:
        if missing_ok:
            return None
        raise
    with os.fdopen(descriptor, "rb") as source:
        initial_metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(initial_metadata.st_mode):
            raise ValueError(f"manifest artifact is not a regular file: {path}")
        contents = source.read()
        os.fsync(source.fileno())
        final_descriptor_metadata = os.fstat(source.fileno())
    final_path_metadata = path.lstat()
    identity_fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns")
    initial_identity = tuple(getattr(initial_metadata, field) for field in identity_fields)
    if (
        not stat.S_ISREG(final_descriptor_metadata.st_mode)
        or not stat.S_ISREG(final_path_metadata.st_mode)
        or initial_identity
        != tuple(getattr(final_descriptor_metadata, field) for field in identity_fields)
        or initial_identity
        != tuple(getattr(final_path_metadata, field) for field in identity_fields)
        or len(contents) != initial_metadata.st_size
    ):
        raise ValueError(f"manifest artifact changed while read: {path}")
    return contents


def attest_artifact(path: Path) -> dict:
    try:
        contents = read_stable_regular(path, missing_ok=True)
    except (OSError, ValueError) as error:
        return {
            "path": str(path),
            "present": False,
            "error": f"{type(error).__name__}: {error}",
        }
    if contents is None:
        return {"path": str(path), "present": False}
    return {
        "path": str(path),
        "present": True,
        "size_bytes": len(contents),
        "sha256": hashlib.sha256(contents).hexdigest(),
    }


summary_bytes = read_stable_regular(final_summary_path)
if summary_bytes is None:
    raise ValueError(f"missing published integrity summary: {final_summary_path}")
artifact_dir = Path(sys.argv[12])
case_artifacts = [
    attest_artifact(artifact_dir / case_id / filename)
    for case_id in ("jsonl", "sqlite")
    for filename in (
        "result.json",
        "output.log",
        "test-log.jsonl",
        "artifact-index.jsonl",
        f"{case_id}-fault-window-summary.json",
    )
]
overall_exit = int(sys.argv[19])
if overall_exit == 0 and not all(artifact.get("present") is True for artifact in case_artifacts):
    raise ValueError("successful run is missing a mandatory attested artifact")
for case_id in ("jsonl", "sqlite"):
    try:
        case_directory_descriptor = os.open(
            artifact_dir / case_id,
            os.O_RDONLY
            | getattr(os, "O_DIRECTORY", 0)
            | getattr(os, "O_NOFOLLOW", 0),
        )
    except OSError:
        if overall_exit == 0:
            raise
        continue
    try:
        os.fsync(case_directory_descriptor)
    finally:
        os.close(case_directory_descriptor)
payload = {
    "schema": "pi.e2e.persistence_fault_injection.manifest.v1",
    "run_id": sys.argv[3],
    "attempt_id": sys.argv[4],
    "correlation_id": sys.argv[3],
    "source_commit": sys.argv[5],
    "source_dirty": sys.argv[6] == "true",
    "source_tree_sha256": sys.argv[7],
    "source_commit_final": sys.argv[8],
    "source_dirty_final": sys.argv[9] == "true",
    "source_tree_sha256_final": sys.argv[10],
    "timestamp": sys.argv[11],
    "artifact_dir": sys.argv[12],
    "runner_mode": sys.argv[13],
    "rch_force_remote": sys.argv[14] == "true",
    "rch_require_remote": sys.argv[15] == "true",
    "execution_attestation": "configuration_only",
    "terminal_state": "complete",
    "overall_passed": overall_exit == 0,
    "result_files": [
        str(artifact_dir / "jsonl/result.json"),
        str(artifact_dir / "sqlite/result.json"),
        str(final_summary_path),
    ],
    "integrity_summary": {
        "path": str(final_summary_path),
        "size_bytes": len(summary_bytes),
        "sha256": hashlib.sha256(summary_bytes).hexdigest(),
    },
    "artifacts": case_artifacts,
    "exit_codes": {
        "jsonl": int(sys.argv[16]),
        "sqlite": int(sys.argv[17]),
        "summary_validation": int(sys.argv[18]),
        "overall": overall_exit,
    },
}
with manifest_path.open("x", encoding="utf-8") as manifest:
    json.dump(payload, manifest, indent=2)
    manifest.write("\n")
    manifest.flush()
    os.fsync(manifest.fileno())
PY

python3 - \
    "$ARTIFACT_DIR/.run-manifest.pending.json" \
    "$ARTIFACT_DIR/run-manifest.json" \
    "$ARTIFACT_DIR" <<'PY'
import json
import os
import stat
import sys
from pathlib import Path

pending = Path(sys.argv[1])
published = Path(sys.argv[2])
artifact_dir = Path(sys.argv[3])
descriptor = os.open(pending, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
with os.fdopen(descriptor, "rb") as source:
    pending_metadata = os.fstat(source.fileno())
    if not stat.S_ISREG(pending_metadata.st_mode):
        raise SystemExit("pending run manifest is not a regular file")
    manifest_bytes = source.read()
    final_descriptor_metadata = os.fstat(source.fileno())
path_metadata = pending.lstat()
if (
    not stat.S_ISREG(path_metadata.st_mode)
    or (pending_metadata.st_dev, pending_metadata.st_ino, pending_metadata.st_size, pending_metadata.st_mtime_ns)
    != (final_descriptor_metadata.st_dev, final_descriptor_metadata.st_ino, final_descriptor_metadata.st_size, final_descriptor_metadata.st_mtime_ns)
    or (pending_metadata.st_dev, pending_metadata.st_ino, pending_metadata.st_size, pending_metadata.st_mtime_ns)
    != (path_metadata.st_dev, path_metadata.st_ino, path_metadata.st_size, path_metadata.st_mtime_ns)
    or len(manifest_bytes) != pending_metadata.st_size
):
    raise SystemExit("pending run manifest changed before publication")
try:
    manifest = json.loads(manifest_bytes)
except (UnicodeDecodeError, json.JSONDecodeError) as error:
    raise SystemExit(f"pending run manifest is invalid JSON: {error}") from error
if (
    not isinstance(manifest, dict)
    or manifest.get("schema") != "pi.e2e.persistence_fault_injection.manifest.v1"
    or manifest.get("terminal_state") != "complete"
):
    raise SystemExit("pending run manifest lacks the terminal completion contract")
if published.exists() or published.is_symlink():
    raise SystemExit("run manifest publication target is unsafe")
os.link(pending, published, follow_symlinks=False)
published_metadata = published.lstat()
if (
    not stat.S_ISREG(published_metadata.st_mode)
    or (published_metadata.st_dev, published_metadata.st_ino)
    != (pending_metadata.st_dev, pending_metadata.st_ino)
):
    raise SystemExit("published run manifest inode does not match attested bytes")
pending.unlink()
directory_descriptor = os.open(
    artifact_dir,
    os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0),
)
try:
    os.fsync(directory_descriptor)
finally:
    os.close(directory_descriptor)
PY

echo "[fault-injection] Completed with exit code $overall_exit"
echo "[fault-injection] Artifacts: $ARTIFACT_DIR"
echo "[fault-injection] Integrity summary: $ARTIFACT_DIR/integrity-summary.json"
echo "[fault-injection] Completion manifest: $ARTIFACT_DIR/run-manifest.json"

exit "$overall_exit"
