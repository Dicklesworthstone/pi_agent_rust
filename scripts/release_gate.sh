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
#   ./scripts/release_gate.sh --require-rch            # require remote offload for cargo checks
#   ./scripts/release_gate.sh --no-rch                 # force local cargo execution
#
# Environment:
#   RELEASE_GATE_MIN_PASS_RATE     Minimum conformance pass rate (default: 80)
#   RELEASE_GATE_MAX_FAIL_COUNT    Maximum conformance failures (default: 36)
#   RELEASE_GATE_MAX_NA_COUNT      Maximum N/A scenarios (default: 170)
#   RELEASE_GATE_MAX_EVIDENCE_AGE_HOURS Maximum source-bound evidence age (default: 168)
#   RELEASE_GATE_REQUIRE_DROPIN_CERTIFIED  Set to 1 to require CERTIFIED drop-in verdict
#   RELEASE_GATE_REQUIRE_PERFORMANCE_CLAIM_READY Set to 1 only when release copy makes
#                                      quantitative/global performance claims (default: 0)
#   RELEASE_GATE_REQUIRE_PREFLIGHT Set to 1 to require preflight analyzer (default: 0)
#   RELEASE_GATE_REQUIRE_QUALITY   Set to 1 to require quality pipeline pass (default: 0)
#   RELEASE_GATE_CARGO_RUNNER      Cargo runner mode: rch | auto | local (default: rch)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ─── Configuration ──────────────────────────────────────────────────────────

MIN_PASS_RATE="${RELEASE_GATE_MIN_PASS_RATE:-80}"
MAX_FAIL_COUNT="${RELEASE_GATE_MAX_FAIL_COUNT:-36}"
MAX_NA_COUNT="${RELEASE_GATE_MAX_NA_COUNT:-170}"
MAX_EVIDENCE_AGE_HOURS="${RELEASE_GATE_MAX_EVIDENCE_AGE_HOURS:-168}"
REQUIRE_DROPIN_CERTIFIED="${RELEASE_GATE_REQUIRE_DROPIN_CERTIFIED:-0}"
REQUIRE_PERFORMANCE_CLAIM_READY="${RELEASE_GATE_REQUIRE_PERFORMANCE_CLAIM_READY:-0}"
REQUIRE_PREFLIGHT="${RELEASE_GATE_REQUIRE_PREFLIGHT:-0}"
REQUIRE_QUALITY="${RELEASE_GATE_REQUIRE_QUALITY:-0}"
CARGO_RUNNER_REQUEST="${RELEASE_GATE_CARGO_RUNNER:-rch}" # rch | auto | local
CARGO_RUNNER_MODE="local"
declare -a CARGO_RUNNER_ARGS=("cargo")
EVIDENCE_DIR=""
REPORT_JSON=0
EVIDENCE_DIR_SELECTION_DETAIL=""
SEEN_NO_RCH=false
SEEN_REQUIRE_RCH=false

for toggle_name in REQUIRE_DROPIN_CERTIFIED REQUIRE_PERFORMANCE_CLAIM_READY REQUIRE_PREFLIGHT REQUIRE_QUALITY; do
    toggle_value="${!toggle_name}"
    if [[ "$toggle_value" != "0" && "$toggle_value" != "1" ]]; then
        echo "Invalid $toggle_name value: $toggle_value (expected: 0|1)" >&2
        exit 2
    fi
done
for threshold_name in MIN_PASS_RATE MAX_FAIL_COUNT MAX_NA_COUNT MAX_EVIDENCE_AGE_HOURS; do
    threshold_value="${!threshold_name}"
    if [[ ! "$threshold_value" =~ ^[0-9]+$ ]]; then
        echo "Invalid $threshold_name value: $threshold_value (expected a non-negative integer)" >&2
        exit 2
    fi
    # Bash arithmetic is signed and treats leading-zero values as octal. Normalize
    # accepted decimal input and reject values that cannot be compared safely.
    normalized_threshold="${threshold_value#"${threshold_value%%[!0]*}"}"
    if [[ -z "$normalized_threshold" ]]; then
        normalized_threshold="0"
    fi
    threshold_too_large=false
    if [[ ${#normalized_threshold} -gt 19 ]]; then
        threshold_too_large=true
    elif [[ ${#normalized_threshold} -eq 19 ]] \
        && [[ "${normalized_threshold:0:1}" == "9" ]] \
        && (( 10#${normalized_threshold:1} > 223372036854775807 )); then
        threshold_too_large=true
    fi
    if [[ "$threshold_too_large" == true ]]; then
        echo "Invalid $threshold_name value: $threshold_value (exceeds signed 64-bit range)" >&2
        exit 2
    fi
    printf -v "$threshold_name" '%s' "$normalized_threshold"
done
if [[ "$MIN_PASS_RATE" -gt 100 ]]; then
    echo "Invalid MIN_PASS_RATE value: $MIN_PASS_RATE (expected: 0..100)" >&2
    exit 2
fi
if [[ "$MAX_EVIDENCE_AGE_HOURS" -eq 0 ]]; then
    echo "Invalid MAX_EVIDENCE_AGE_HOURS value: 0 (expected at least one hour)" >&2
    exit 2
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --evidence-dir)
            if [[ $# -lt 2 ]] || [[ -z "$2" ]] || [[ "$2" == --* ]]; then
                echo "--evidence-dir requires a non-empty path" >&2
                exit 2
            fi
            EVIDENCE_DIR="$2"
            shift 2
            ;;
        --report) REPORT_JSON=1; shift ;;
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
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *) echo "Unknown flag: $1"; exit 1 ;;
    esac
done

# ─── Cargo Runner Resolution ────────────────────────────────────────────────

if [[ "$CARGO_RUNNER_REQUEST" != "rch" && "$CARGO_RUNNER_REQUEST" != "auto" && "$CARGO_RUNNER_REQUEST" != "local" ]]; then
    echo "Invalid RELEASE_GATE_CARGO_RUNNER value: $CARGO_RUNNER_REQUEST (expected: rch|auto|local)" >&2
    exit 2
fi

if [[ "$CARGO_RUNNER_REQUEST" == "rch" ]]; then
    if ! command -v rch >/dev/null 2>&1; then
        echo "RELEASE_GATE_CARGO_RUNNER=rch requested, but 'rch' is not available in PATH." >&2
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

# Auto-detect latest complete evidence directory if not specified.
if [[ -z "$EVIDENCE_DIR" ]]; then
    E2E_RESULTS="$PROJECT_ROOT/tests/e2e_results"
    if [[ -d "$E2E_RESULTS" ]]; then
        # "Complete" currently means the run produced the required gate artifact(s).
        # Add additional required files here as the evidence contract evolves.
        required_artifacts=("evidence_contract.json" "environment.json" "summary.json")
        skipped_count=0
        declare -a skipped_examples=()

        while IFS= read -r candidate; do
            [[ -z "$candidate" ]] && continue
            candidate_name="${candidate##*/}"
            [[ "$candidate_name" =~ ^[0-9]{8}T[0-9]{6}Z$ ]] || continue

            missing_artifacts=()
            for artifact in "${required_artifacts[@]}"; do
                if [[ ! -f "$candidate/$artifact" ]]; then
                    missing_artifacts+=("$artifact")
                fi
            done

            if [[ ${#missing_artifacts[@]} -eq 0 ]]; then
                EVIDENCE_DIR="$candidate"
                if [[ "$skipped_count" -gt 0 ]]; then
                    EVIDENCE_DIR_SELECTION_DETAIL="Selected ${candidate#"$PROJECT_ROOT"/} after skipping $skipped_count incomplete newer run(s): ${skipped_examples[*]}"
                fi
                break
            fi

            skipped_count=$((skipped_count + 1))
            if [[ ${#skipped_examples[@]} -lt 3 ]]; then
                missing_csv="$(IFS=,; echo "${missing_artifacts[*]}")"
                skipped_examples+=("${candidate#"$PROJECT_ROOT"/} (missing: $missing_csv)")
            fi
        done < <(find "$E2E_RESULTS" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort -r)

        if [[ -z "$EVIDENCE_DIR" ]] && [[ "$skipped_count" -gt 0 ]]; then
            EVIDENCE_DIR_SELECTION_DETAIL="No complete evidence bundle found under tests/e2e_results; skipped $skipped_count incomplete run(s): ${skipped_examples[*]}"
        fi
    fi
fi

# ─── State tracking ─────────────────────────────────────────────────────────

PASS_COUNT=0
FAIL_COUNT=0
WARN_COUNT=0
declare -a CHECKS=()

json_string() {
    python3 -c 'import json, sys; print(json.dumps(sys.stdin.buffer.read().decode("utf-8", "surrogateescape")))'
}

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
    CHECKS+=("{\"name\":$(printf '%s' "$name" | json_string),\"status\":\"pass\",\"detail\":$(printf '%s' "$detail" | json_string)}")
}

check_fail() {
    local name="$1"
    local detail="$2"
    log "FAIL" "$name: $detail"
    FAIL_COUNT=$((FAIL_COUNT + 1))
    CHECKS+=("{\"name\":$(printf '%s' "$name" | json_string),\"status\":\"fail\",\"detail\":$(printf '%s' "$detail" | json_string)}")
}

check_warn() {
    local name="$1"
    local detail="$2"
    log "WARN" "$name: $detail"
    WARN_COUNT=$((WARN_COUNT + 1))
    CHECKS+=("{\"name\":$(printf '%s' "$name" | json_string),\"status\":\"warn\",\"detail\":$(printf '%s' "$detail" | json_string)}")
}

run_cargo_gate() {
    "${CARGO_RUNNER_ARGS[@]}" "$@"
}

validate_exact_libtest_output() {
    local mode="$1"
    local test_name="$2"
    python3 -c '
import re
import sys

mode, test_name = sys.argv[1:]
lines = [line.strip() for line in sys.stdin.read().splitlines()]
if mode == "list":
    listed = [line for line in lines if line.endswith(": test")]
    benchmarks = [
        line
        for line in lines
        if line.endswith(": benchmark") or line.endswith(": bench")
    ]
    summaries = [
        line
        for line in lines
        if re.fullmatch(r"[0-9]+ tests?, [0-9]+ benchmarks?", line)
    ]
    if (
        listed != [f"{test_name}: test"]
        or benchmarks
        or summaries not in ([], ["1 test, 0 benchmarks"])
    ):
        raise SystemExit(1)
elif mode == "run":
    running = [
        match.group(1)
        for line in lines
        if (match := re.fullmatch(r"running ([0-9]+) tests?", line)) is not None
    ]
    results = [line for line in lines if line.startswith("test result:")]
    result_pattern = re.compile(
        r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
        r"[0-9]+ filtered out; finished in .+"
    )
    if running != ["1"] or len(results) != 1 or result_pattern.fullmatch(results[0]) is None:
        raise SystemExit(1)
else:
    raise SystemExit(2)
' "$mode" "$test_name"
}

capture_repository_snapshot() {
    python3 - "$PROJECT_ROOT" <<'PY'
import hashlib
import os
import stat
import subprocess
import sys
from pathlib import Path

raw_root = Path(sys.argv[1])


def fail(detail):
    print(detail, file=sys.stderr)
    raise SystemExit(1)


if raw_root.is_symlink() or not raw_root.is_dir():
    fail("repository root must be a real directory, not a symlink")
try:
    root = raw_root.resolve(strict=True)
    git_marker = root / ".git"
    if git_marker.is_symlink():
        fail("repository .git marker must not be a symlink")
    if git_marker.is_dir():
        git_dir = git_marker.resolve(strict=True)
    elif git_marker.is_file():
        marker = git_marker.read_text(encoding="utf-8").rstrip("\r\n")
        if "\n" in marker or "\r" in marker or not marker.startswith("gitdir: "):
            fail("repository .git file is malformed")
        target = Path(marker.removeprefix("gitdir: "))
        git_dir = (target if target.is_absolute() else root / target).resolve(strict=True)
    else:
        fail("repository .git marker is missing")
except (OSError, RuntimeError, UnicodeError) as exc:
    fail(f"repository Git context could not be resolved safely: {exc}")
if not git_dir.is_dir():
    fail("repository Git directory is not a directory")


def git(*args):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_LITERAL_PATHSPECS"] = "1"
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    env["GIT_OPTIONAL_LOCKS"] = "0"
    env["GIT_TERMINAL_PROMPT"] = "0"
    result = subprocess.run(
        [
            "git",
            "--git-dir", str(git_dir),
            "--work-tree", str(root),
            "-c", "core.bare=false",
            "-c", "core.fsmonitor=false",
            "-c", f"core.worktree={root}",
            *args,
        ],
        capture_output=True,
        env=env,
        check=False,
    )
    if result.returncode != 0:
        diagnostic = result.stderr.decode("utf-8", "replace").strip()
        fail(f"git {' '.join(args)} failed: {diagnostic}")
    return result.stdout


def split_record(record, label):
    try:
        metadata, path = record.split(b"\t", 1)
    except ValueError:
        fail(f"malformed {label} record")
    if not path:
        fail(f"empty path in {label} record")
    return metadata, path


top_level_text = git("rev-parse", "--show-toplevel").decode("utf-8", "strict")
if not top_level_text.endswith("\n") or "\n" in top_level_text.removesuffix("\n"):
    fail("Git top-level output is not one canonical line")
top_level = Path(top_level_text.removesuffix("\n")).resolve(strict=True)
if top_level != root:
    fail("Git worktree does not match the canonical release repository root")

absolute_git_dir_text = git("rev-parse", "--absolute-git-dir").decode("utf-8", "strict")
if not absolute_git_dir_text.endswith("\n") or "\n" in absolute_git_dir_text.removesuffix("\n"):
    fail("Git absolute directory output is not one canonical line")
reported_git_dir = Path(absolute_git_dir_text.removesuffix("\n")).resolve(strict=True)
if reported_git_dir != git_dir:
    fail("Git directory does not match the canonical release repository binding")

head = git("rev-parse", "--verify", "HEAD^{commit}").decode("ascii", "strict").strip()
if len(head) not in (40, 64) or head.lower() != head or any(ch not in "0123456789abcdef" for ch in head):
    fail(f"HEAD is not a canonical full object ID: {head!r}")

tree_bytes = git("ls-tree", "-r", "-z", "--full-tree", head)
tree_digest = hashlib.sha256(tree_bytes).hexdigest()
tree = {}
for record in filter(None, tree_bytes.split(b"\0")):
    metadata, path = split_record(record, "tree")
    fields = metadata.split(b" ")
    if len(fields) != 3:
        fail("malformed tree metadata")
    mode, object_type, oid = fields
    if mode not in (b"100644", b"100755", b"120000") or object_type != b"blob":
        fail(f"unsupported tracked entry at {os.fsdecode(path)!r}: mode={mode!r} type={object_type!r}")
    if path in tree:
        fail(f"duplicate path in HEAD tree: {os.fsdecode(path)!r}")
    tree[path] = (mode, oid)

index_bytes = git("ls-files", "--stage", "-z")
index = {}
for record in filter(None, index_bytes.split(b"\0")):
    metadata, path = split_record(record, "index")
    fields = metadata.split(b" ")
    if len(fields) != 3:
        fail("malformed index metadata")
    mode, oid, stage = fields
    if stage != b"0":
        fail(f"non-stage-zero index entry at {os.fsdecode(path)!r}")
    if path in index:
        fail(f"duplicate path in index: {os.fsdecode(path)!r}")
    index[path] = (mode, oid)
if index != tree:
    fail("index entries do not match the release HEAD tree exactly")

flag_bytes = git("ls-files", "-v", "-z")
flag_paths = set()
for record in filter(None, flag_bytes.split(b"\0")):
    if len(record) < 3 or record[1:2] != b" ":
        fail("malformed index-flag record")
    tag, path = record[:1], record[2:]
    if tag != b"H":
        fail(
            f"non-canonical index flag {tag.decode('ascii', 'replace')!r} at {os.fsdecode(path)!r}; "
            "assume-unchanged and skip-worktree are forbidden for a release"
        )
    flag_paths.add(path)
if flag_paths != set(tree):
    fail("index-flag path set does not match the release HEAD tree")

untracked_bytes = git("ls-files", "--others", "--exclude-standard", "-z")
if untracked_bytes:
    paths = [os.fsdecode(path) for path in untracked_bytes.split(b"\0") if path]
    fail("untracked non-ignored paths are present: " + ", ".join(paths[:10]))

root_bytes = os.fsencode(root)


def framed_field(value):
    return str(len(value)).encode("ascii") + b":" + value


def capture_raw_worktree_digest():
    digest = hashlib.sha256()
    for path, (mode, expected_oid) in tree.items():
        full_path = os.path.join(root_bytes, path)
        parent = os.path.dirname(full_path)
        while parent != root_bytes:
            try:
                parent_stat = os.lstat(parent)
            except OSError as exc:
                fail(f"cannot inspect parent of {os.fsdecode(path)!r}: {exc}")
            if stat.S_ISLNK(parent_stat.st_mode):
                fail(f"tracked path traverses a symlinked parent: {os.fsdecode(path)!r}")
            next_parent = os.path.dirname(parent)
            if next_parent == parent or not parent.startswith(root_bytes + os.sep.encode()):
                fail(f"tracked path escapes repository root: {os.fsdecode(path)!r}")
            parent = next_parent

        try:
            file_stat = os.lstat(full_path)
        except OSError as exc:
            fail(f"cannot inspect tracked path {os.fsdecode(path)!r}: {exc}")
        if mode in (b"100644", b"100755"):
            if not stat.S_ISREG(file_stat.st_mode):
                fail(f"tracked regular file has wrong worktree type: {os.fsdecode(path)!r}")
            actual_mode = b"100755" if file_stat.st_mode & 0o111 else b"100644"
            if actual_mode != mode:
                fail(
                    f"raw worktree mode differs from release HEAD at {os.fsdecode(path)!r}: "
                    f"expected={mode.decode('ascii')} actual={actual_mode.decode('ascii')}"
                )
            try:
                with open(full_path, "rb") as handle:
                    contents = handle.read()
            except OSError as exc:
                fail(f"cannot read tracked path {os.fsdecode(path)!r}: {exc}")
        else:
            if not stat.S_ISLNK(file_stat.st_mode):
                fail(f"tracked symlink has wrong worktree type: {os.fsdecode(path)!r}")
            actual_mode = b"120000"
            try:
                contents = os.readlink(full_path)
            except OSError as exc:
                fail(f"cannot read tracked symlink {os.fsdecode(path)!r}: {exc}")

        framed_blob = b"blob " + str(len(contents)).encode("ascii") + b"\0" + contents
        if len(expected_oid) == 40:
            actual_oid = hashlib.sha1(framed_blob).hexdigest().encode("ascii")
        elif len(expected_oid) == 64:
            actual_oid = hashlib.sha256(framed_blob).hexdigest().encode("ascii")
        else:
            fail(f"unsupported Git object ID length for {os.fsdecode(path)!r}")
        if actual_oid != expected_oid:
            fail(f"raw worktree bytes differ from release HEAD at {os.fsdecode(path)!r}")

        digest.update(framed_field(path))
        digest.update(framed_field(actual_mode))
        digest.update(framed_field(contents))
    return digest.hexdigest()


def verify_git_metadata_unchanged():
    if git("rev-parse", "--verify", "HEAD^{commit}").decode("ascii", "strict").strip() != head:
        fail("HEAD changed while repository state was captured")
    if git("ls-files", "--stage", "-z") != index_bytes:
        fail("index changed while repository state was captured")
    if git("ls-files", "-v", "-z") != flag_bytes:
        fail("index flags changed while repository state was captured")
    if git("ls-files", "--others", "--exclude-standard", "-z") != untracked_bytes:
        fail("untracked path set changed while repository state was captured")


initial_worktree_digest = capture_raw_worktree_digest()
verify_git_metadata_unchanged()
final_worktree_digest = capture_raw_worktree_digest()
if final_worktree_digest != initial_worktree_digest:
    fail("raw tracked worktree bytes or modes changed while repository state was captured")
verify_git_metadata_unchanged()

print(f"{head}|{tree_digest}|{final_worktree_digest}")
PY
}

# ─── Gate checks ────────────────────────────────────────────────────────────

# Emit evidence-directory selection diagnostics before gate checks.
if [[ -n "$EVIDENCE_DIR_SELECTION_DETAIL" ]]; then
    check_warn "evidence_dir_selection" "$EVIDENCE_DIR_SELECTION_DETAIL"
fi
check_pass "cargo_runner" "mode=$CARGO_RUNNER_MODE request=$CARGO_RUNNER_REQUEST"

INITIAL_REPOSITORY_SNAPSHOT=""
if INITIAL_REPOSITORY_SNAPSHOT=$(capture_repository_snapshot 2>&1); then
    check_pass "initial_repository_state" "Source is byte-for-byte clean at ${INITIAL_REPOSITORY_SNAPSHOT%%|*}"
else
    check_fail "initial_repository_state" "Release source is not byte-for-byte clean: $INITIAL_REPOSITORY_SNAPSHOT"
    INITIAL_REPOSITORY_SNAPSHOT=""
fi

# Gate 1: Evidence directory exists
if [[ -z "$EVIDENCE_DIR" ]] || [[ ! -d "$EVIDENCE_DIR" ]]; then
    if [[ -n "$EVIDENCE_DIR_SELECTION_DETAIL" ]]; then
        check_fail "evidence_dir" "No evidence directory found. $EVIDENCE_DIR_SELECTION_DETAIL"
    else
        check_fail "evidence_dir" "No evidence directory found"
    fi
else
    check_pass "evidence_dir" "Found: $EVIDENCE_DIR"
fi

# Gate 2: Evidence contract
EVIDENCE_CONTRACT="$EVIDENCE_DIR/evidence_contract.json"
if [[ -f "$EVIDENCE_CONTRACT" ]]; then
    if EVIDENCE_CHECK=$(python3 - "$PROJECT_ROOT" "$EVIDENCE_DIR" "$MAX_EVIDENCE_AGE_HOURS" 2>&1 <<'PY'
import fnmatch
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import tomllib
from datetime import datetime, timedelta, timezone
from pathlib import Path

raw_project_root = Path(sys.argv[1])
evidence_dir = Path(sys.argv[2])
maximum_age = timedelta(hours=int(sys.argv[3]))

def finish(status, detail):
    print(f"{status}|{detail}")
    raise SystemExit(0)

def reject_duplicate_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object key: {key}")
        value[key] = item
    return value

def resolve_repository_context():
    if raw_project_root.is_symlink() or not raw_project_root.is_dir():
        finish("fail", "E2E repository root must be a real directory, not a symlink")
    try:
        resolved_root = raw_project_root.resolve(strict=True)
        git_marker = resolved_root / ".git"
        marker_metadata = git_marker.lstat()
        if stat.S_ISLNK(marker_metadata.st_mode):
            finish("fail", "E2E repository .git marker must not be a symlink")
        if stat.S_ISDIR(marker_metadata.st_mode):
            resolved_git_dir = git_marker.resolve(strict=True)
        elif stat.S_ISREG(marker_metadata.st_mode):
            marker = git_marker.read_text(encoding="utf-8").rstrip("\r\n")
            if "\n" in marker or "\r" in marker or not marker.startswith("gitdir: "):
                finish("fail", "E2E repository .git file is malformed")
            target = Path(marker.removeprefix("gitdir: "))
            candidate = target if target.is_absolute() else resolved_root / target
            target_metadata = candidate.lstat()
            if stat.S_ISLNK(target_metadata.st_mode) or not stat.S_ISDIR(target_metadata.st_mode):
                finish("fail", "E2E repository gitfile target must be a non-symlink directory")
            resolved_git_dir = candidate.resolve(strict=True)
        else:
            finish("fail", "E2E repository .git marker is not a directory or gitfile")
    except (OSError, RuntimeError, UnicodeError) as exc:
        finish("fail", f"E2E repository Git context could not be resolved safely: {exc}")
    if not resolved_git_dir.is_dir():
        finish("fail", "E2E repository Git directory is not a directory")
    return resolved_root, resolved_git_dir

def load_object(path, label):
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        finish("fail", f"{label} is missing: {path}")
    except OSError as exc:
        finish("fail", f"unable to inspect {label}: {exc}")
    if not stat.S_ISREG(metadata.st_mode):
        finish("fail", f"{label} must be a regular file, not a symlink or special file: {path}")
    if os.name != "nt" and metadata.st_mode & 0o111:
        finish("fail", f"{label} must not be executable: {path}")
    try:
        raw_bytes = path.read_bytes()
        payload = json.loads(
            raw_bytes.decode("utf-8"),
            object_pairs_hook=reject_duplicate_keys,
        )
    except Exception as exc:  # noqa: BLE001
        finish("fail", f"{label} is not valid JSON: {exc}")
    if not isinstance(payload, dict):
        finish("fail", f"{label} root must be an object")
    return payload, raw_bytes

def uint(value, label):
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= 2**63 - 1:
        finish("fail", f"{label} must be a non-negative signed 64-bit integer")
    return value

def string_list(value, label, *, nonempty=False):
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        finish("fail", f"{label} must be an array of non-empty strings")
    if nonempty and not value:
        finish("fail", f"{label} must not be empty")
    if len(value) != len(set(value)):
        finish("fail", f"{label} must not contain duplicates")
    return value

def run_git(*args, text=True):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_LITERAL_PATHSPECS"] = "1"
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    env["GIT_OPTIONAL_LOCKS"] = "0"
    env["GIT_TERMINAL_PROMPT"] = "0"
    return subprocess.run(
        [
            "git",
            "--git-dir", str(git_dir),
            "--work-tree", str(project_root),
            "-c", "core.bare=false",
            "-c", "core.fsmonitor=false",
            "-c", f"core.worktree={project_root}",
            *args,
        ],
        capture_output=True,
        text=text,
        env=env,
        check=False,
    )

def package_includes(path, patterns):
    for raw_pattern in patterns:
        if not isinstance(raw_pattern, str) or not raw_pattern:
            finish("fail", "source Cargo.toml package.include entries must be non-empty strings")
        pattern = raw_pattern.removeprefix("/")
        if fnmatch.fnmatchcase(path, pattern):
            return True
        if pattern.endswith("/**") and path.startswith(pattern[:-3].rstrip("/") + "/"):
            return True
    return False

project_root, git_dir = resolve_repository_context()

def verify_repository_binding():
    bindings = (
        (("rev-parse", "--show-toplevel"), project_root, "worktree"),
        (("rev-parse", "--absolute-git-dir"), git_dir, "Git directory"),
    )
    for args, expected, label in bindings:
        result = run_git(*args)
        if result.returncode != 0:
            finish("fail", f"unable to verify canonical E2E repository {label}")
        output = result.stdout
        if not output.endswith("\n") or "\n" in output.removesuffix("\n"):
            finish("fail", f"E2E repository {label} output is not one canonical line")
        try:
            reported = Path(output.removesuffix("\n")).resolve(strict=True)
        except (OSError, RuntimeError) as exc:
            finish("fail", f"unable to canonicalize E2E repository {label}: {exc}")
        if reported != expected:
            finish("fail", f"E2E repository {label} does not match the filesystem-derived binding")

verify_repository_binding()

def framed_field(value):
    return str(len(value)).encode("ascii") + b":" + value

def recompute_e2e_source_snapshot(commit):
    tree_result = run_git(
        "ls-tree",
        "-r",
        "-z",
        "--full-tree",
        commit,
        text=False,
    )
    if tree_result.returncode != 0:
        finish("fail", "unable to enumerate the E2E source tree")
    tree_bytes = tree_result.stdout
    entries = []
    seen_paths = set()
    canonical_index = bytearray()
    canonical_flags = bytearray()
    for record in (item for item in tree_bytes.split(b"\0") if item):
        try:
            metadata, path = record.split(b"\t", 1)
            mode, object_type, object_id = metadata.split(b" ", 2)
        except ValueError:
            finish("fail", "E2E source tree contains a malformed record")
        if (
            not path
            or path in seen_paths
            or mode not in (b"100644", b"100755", b"120000")
            or object_type != b"blob"
            or len(object_id) not in (40, 64)
            or object_id.lower() != object_id
            or any(byte not in b"0123456789abcdef" for byte in object_id)
        ):
            finish("fail", f"E2E source tree contains a non-canonical entry at {os.fsdecode(path)!r}")
        seen_paths.add(path)
        entries.append((path, mode, object_id))
        canonical_index.extend(mode + b" " + object_id + b" 0\t" + path + b"\0")
        canonical_flags.extend(b"H " + path + b"\0")

    digest = hashlib.sha256()
    digest.update(framed_field(b"pi.e2e.source_snapshot.v1"))
    digest.update(framed_field(commit.encode("ascii")))
    digest.update(framed_field(tree_bytes))
    digest.update(framed_field(bytes(canonical_index)))
    digest.update(framed_field(bytes(canonical_flags)))
    for path, mode, object_id in entries:
        blob = run_git("cat-file", "blob", os.fsdecode(object_id), text=False)
        if blob.returncode != 0:
            finish("fail", f"unable to read E2E source blob at {os.fsdecode(path)!r}")
        framed_blob = b"blob " + str(len(blob.stdout)).encode("ascii") + b"\0" + blob.stdout
        if len(object_id) == 40:
            actual_object_id = hashlib.sha1(framed_blob).hexdigest().encode("ascii")
        else:
            actual_object_id = hashlib.sha256(framed_blob).hexdigest().encode("ascii")
        if actual_object_id != object_id:
            finish("fail", f"E2E source blob identity is corrupt at {os.fsdecode(path)!r}")
        digest.update(framed_field(path))
        digest.update(framed_field(mode))
        digest.update(framed_field(blob.stdout))
    return "sha256:" + digest.hexdigest()

try:
    root_resolved = project_root.resolve(strict=True)
    results_root = (project_root / "tests/e2e_results").resolve(strict=True)
    evidence_resolved = evidence_dir.resolve(strict=True)
except (OSError, RuntimeError) as exc:
    finish("fail", f"unable to resolve E2E evidence path: {exc}")
if evidence_dir.is_symlink() or not evidence_resolved.is_dir() or evidence_resolved.parent != results_root:
    finish("fail", "E2E evidence directory must be a direct, non-symlinked child of tests/e2e_results")
if re.fullmatch(r"[0-9]{8}T[0-9]{6}Z", evidence_resolved.name) is None:
    finish("fail", "E2E evidence directory name must use the canonical YYYYMMDDTHHMMSSZ format")
try:
    evidence_resolved.relative_to(root_resolved)
except ValueError:
    finish("fail", "E2E evidence directory resolves outside the repository")

contract_path = evidence_resolved / "evidence_contract.json"
environment_path = evidence_resolved / "environment.json"
summary_path = evidence_resolved / "summary.json"
contract, contract_bytes = load_object(contract_path, "evidence contract")
environment, environment_bytes = load_object(environment_path, "E2E environment")
summary, summary_bytes = load_object(summary_path, "E2E summary")
decision_inputs = [
    (contract_path, contract_bytes),
    (environment_path, environment_bytes),
    (summary_path, summary_bytes),
]

if contract.get("schema") != "pi.evidence.contract.v1":
    finish("fail", f"unsupported evidence contract schema: {contract.get('schema')!r}")
if environment.get("schema") != "pi.e2e.environment.v1":
    finish("fail", f"unsupported E2E environment schema: {environment.get('schema')!r}")
if summary.get("schema") != "pi.e2e.summary.v1":
    finish("fail", f"unsupported E2E summary schema: {summary.get('schema')!r}")

generated_values = (
    contract.get("generated_at"),
    environment.get("generated_at"),
    summary.get("generated_at"),
)
if not all(isinstance(value, str) for value in generated_values):
    finish("fail", "E2E contract, environment, and summary must each contain generated_at")
if len(set(generated_values)) != 1:
    finish("fail", "E2E contract, environment, and summary generated_at values do not match")
generated_at_raw = generated_values[0]
if re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z", generated_at_raw) is None:
    finish("fail", "E2E generated_at must use canonical UTC second precision")
try:
    generated_at = datetime.fromisoformat(generated_at_raw.removesuffix("Z") + "+00:00")
except ValueError:
    finish("fail", "E2E generated_at is not a valid UTC timestamp")
now = datetime.now(timezone.utc)
if generated_at > now + timedelta(minutes=5):
    finish("fail", "E2E generated_at is more than five minutes in the future")
if now - generated_at > maximum_age:
    finish("fail", f"E2E evidence is stale ({now - generated_at} old; maximum {maximum_age})")
expected_directory_name = generated_at.strftime("%Y%m%dT%H%M%SZ")
if evidence_resolved.name != expected_directory_name:
    finish("fail", "E2E generated_at does not match the canonical evidence directory name")

profile = contract.get("profile")
if profile not in ("ci", "full"):
    finish("fail", f"release evidence profile must be ci or full, got {profile!r}")
if environment.get("profile") != profile or summary.get("profile") != profile:
    finish("fail", "evidence contract, environment, and summary profiles do not match")
expected_strict_conformance = profile == "full"
if contract.get("strict_conformance") is not expected_strict_conformance:
    finish(
        "fail",
        f"strict_conformance must be {str(expected_strict_conformance).lower()} for profile={profile}",
    )
if summary.get("rerun_from") is not None or environment.get("rerun_from") is not None:
    finish("fail", "release evidence must be a baseline run, not a failed-suite rerun")
expected_shard = {"kind": "none", "name": "unsharded", "index": None, "total": None}
if environment.get("shard") != expected_shard or summary.get("shard") != expected_shard:
    finish("fail", "release evidence must be one complete, unsharded run")

if contract.get("status") != "pass":
    finish("fail", f"evidence contract status={contract.get('status')!r} (expected 'pass')")
errors = contract.get("errors")
if not isinstance(errors, list) or errors:
    finish("fail", "evidence contract errors must be an empty array")
checks = contract.get("checks")
if not isinstance(checks, list) or not checks:
    finish("fail", "evidence contract checks must be a non-empty array")
seen_check_ids = set()
checks_by_id = {}
passed_checks = 0
for index, check in enumerate(checks):
    if not isinstance(check, dict):
        finish("fail", f"evidence contract check[{index}] must be an object")
    check_id = check.get("id")
    if not isinstance(check_id, str) or not check_id:
        finish("fail", f"evidence contract check[{index}].id must be a non-empty string")
    if check_id in seen_check_ids:
        finish("fail", f"evidence contract contains duplicate check id: {check_id}")
    seen_check_ids.add(check_id)
    checks_by_id[check_id] = check
    if not isinstance(check.get("path"), str) or not check["path"]:
        finish("fail", f"evidence contract check {check_id} has an invalid path")
    if not isinstance(check.get("diagnostics"), str):
        finish("fail", f"evidence contract check {check_id} has invalid diagnostics")
    if check.get("ok") is not True:
        finish("fail", f"evidence contract check failed: {check_id}")
    passed_checks += 1
if passed_checks != len(checks):
    finish("fail", "evidence contract pass count is inconsistent with its checks")

correlation_id = contract.get("correlation_id")
if not isinstance(correlation_id, str) or not correlation_id:
    finish("fail", "evidence contract correlation_id must be a non-empty string")
if environment.get("correlation_id") != correlation_id or summary.get("correlation_id") != correlation_id:
    finish("fail", "evidence contract, environment, and summary correlation IDs do not match")
artifact_dir = contract.get("artifact_dir")
if not isinstance(artifact_dir, str) or not artifact_dir:
    finish("fail", "evidence contract artifact_dir must be a non-empty string")
if environment.get("artifact_dir") != artifact_dir or summary.get("artifact_dir") != artifact_dir:
    finish("fail", "evidence contract, environment, and summary artifact_dir values do not match")
try:
    if Path(artifact_dir).resolve(strict=True) != evidence_resolved:
        finish("fail", "E2E artifact_dir does not identify the selected evidence directory")
except (OSError, RuntimeError) as exc:
    finish("fail", f"unable to resolve E2E artifact_dir: {exc}")

total_units = uint(summary.get("total_units"), "summary.total_units")
passed_units = uint(summary.get("passed_units"), "summary.passed_units")
failed_units = uint(summary.get("failed_units"), "summary.failed_units")
unit_results = summary.get("unit_targets")
if not isinstance(unit_results, list) or not unit_results:
    finish("fail", "summary.unit_targets must contain actual integration-test results")
if total_units != len(unit_results) or passed_units + failed_units != total_units:
    finish("fail", "summary unit totals are internally inconsistent")
if failed_units != 0 or passed_units != total_units:
    finish("fail", "release evidence contains failed integration-test targets")
if summary.get("failed_unit_names") != []:
    finish("fail", "summary.failed_unit_names must be an empty array")
environment_units = string_list(environment.get("unit_targets"), "environment.unit_targets", nonempty=True)
observed_unit_names = []
required_result_fields = {
    "schema",
    "result_kind",
    "correlation_id",
    "exit_code",
    "duration_ms",
    "passed",
    "failed",
    "ignored",
    "total",
    "log_file",
    "diagnostic_artifacts",
    "timestamp",
}
structured_result_fields = {"test_log_jsonl", "artifact_index_jsonl"}
diagnostic_limits = {
    "output_log": 8 * 1024 * 1024,
    "test_log_jsonl": 4 * 1024 * 1024,
    "artifact_index_jsonl": 4 * 1024 * 1024,
}
leak_patterns = (
    ("openai_like_key", re.compile(r"\bsk-[A-Za-z0-9_-]{20,}\b")),
    ("generic_key_token", re.compile(r"\bkey-[A-Za-z0-9_-]{20,}\b", re.IGNORECASE)),
    ("bearer_token", re.compile(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{16,}")),
    (
        "auth_header_value",
        re.compile(
            r"(?i)(authorization|x-api-key|api[_-]?key|access[_-]?token|refresh[_-]?token)"
            r"\s*[:=]\s*[\"']?[A-Za-z0-9._~+/=-]{12,}"
        ),
    ),
)

def validate_result_diagnostics(result, kind, name, base_dir, *, structured):
    expected_paths = {
        "log_file": base_dir / "output.log",
    }
    if structured:
        expected_paths.update(
            {
                "test_log_jsonl": base_dir / "test-log.jsonl",
                "artifact_index_jsonl": base_dir / "artifact-index.jsonl",
            }
        )
    diagnostic_artifacts = result.get("diagnostic_artifacts")
    if not isinstance(diagnostic_artifacts, dict):
        finish("fail", f"{kind} {name} diagnostic_artifacts must be an object")
    if set(diagnostic_artifacts) != {
        "schema",
        "output_log",
        "test_log_jsonl",
        "artifact_index_jsonl",
    } or diagnostic_artifacts.get("schema") != "pi.e2e.diagnostic_artifacts.v1":
        finish("fail", f"{kind} {name} diagnostic_artifacts has an invalid contract")

    captured = {}
    field_bindings = {
        "log_file": "output_log",
        "test_log_jsonl": "test_log_jsonl",
        "artifact_index_jsonl": "artifact_index_jsonl",
    }
    if not structured:
        for binding_field in ("test_log_jsonl", "artifact_index_jsonl"):
            if diagnostic_artifacts.get(binding_field) is not None:
                finish("fail", f"{kind} {name} {binding_field} binding must be null")
    for field, expected_path in expected_paths.items():
        binding_field = field_bindings[field]
        binding = diagnostic_artifacts.get(binding_field)
        if not isinstance(binding, dict) or set(binding) != {"path", "sha256", "size_bytes"}:
            finish("fail", f"{kind} {name} {binding_field} byte binding is invalid")
        raw_path = result.get(field)
        if not isinstance(raw_path, str) or not raw_path:
            finish("fail", f"{kind} {name} {field} must be a non-empty path")
        try:
            resolved = Path(raw_path).resolve(strict=True)
            expected = expected_path.resolve(strict=True)
            metadata = expected_path.lstat()
            raw = expected_path.read_bytes()
        except (OSError, RuntimeError) as exc:
            finish("fail", f"unable to inspect {kind} {name} {field}: {exc}")
        if resolved != expected:
            finish("fail", f"{kind} {name} {field} does not name its canonical in-run artifact")
        if not stat.S_ISREG(metadata.st_mode) or (os.name != "nt" and metadata.st_mode & 0o111):
            finish("fail", f"{kind} {name} {field} must be a non-executable regular file")
        if binding.get("path") != raw_path:
            finish("fail", f"{kind} {name} {binding_field} binding path does not match its result path")
        bound_sha256 = binding.get("sha256")
        bound_size = binding.get("size_bytes")
        if not isinstance(bound_sha256, str) or re.fullmatch(r"sha256:[0-9a-f]{64}", bound_sha256) is None:
            finish("fail", f"{kind} {name} {binding_field} binding SHA-256 is malformed")
        if isinstance(bound_size, bool) or not isinstance(bound_size, int) or not 0 <= bound_size <= 2**63 - 1:
            finish("fail", f"{kind} {name} {binding_field} binding size is invalid")
        if len(raw) > diagnostic_limits[binding_field]:
            finish(
                "fail",
                f"{kind} {name} {binding_field} exceeds its {diagnostic_limits[binding_field]}-byte budget",
            )
        actual_sha256 = "sha256:" + hashlib.sha256(raw).hexdigest()
        if bound_size != len(raw) or bound_sha256 != actual_sha256:
            finish("fail", f"{kind} {name} {binding_field} byte binding does not match retained bytes")
        text_value = raw.decode("utf-8", "replace")
        for leak_label, leak_pattern in leak_patterns:
            for match in leak_pattern.finditer(text_value):
                snippet = match.group(0)
                upper = snippet.upper()
                if "REDACTED" not in upper and "<TRACE_ID>" not in snippet and "<TIMESTAMP>" not in snippet:
                    finish("fail", f"{kind} {name} {binding_field} contains a potential {leak_label} secret")
        captured[field] = raw
        decision_inputs.append((expected_path, raw))
    if not captured["log_file"]:
        finish("fail", f"{kind} {name} output.log must be non-empty")

    def parse_jsonl(field, *, require_harness):
        raw = captured[field]
        if raw and not raw.endswith(b"\n"):
            finish("fail", f"{kind} {name} {field} must be newline-terminated JSONL")
        lines = raw.removesuffix(b"\n").split(b"\n") if raw else []
        saw_harness = False
        for line_number, line in enumerate(lines, start=1):
            if not line:
                finish("fail", f"{kind} {name} {field} contains an empty JSONL record")
            try:
                payload = json.loads(
                    line.decode("utf-8", "strict"),
                    object_pairs_hook=reject_duplicate_keys,
                )
            except (UnicodeError, json.JSONDecodeError, ValueError) as exc:
                finish("fail", f"{kind} {name} {field} line {line_number} is invalid: {exc}")
            if not isinstance(payload, dict):
                finish("fail", f"{kind} {name} {field} line {line_number} must be an object")
            if payload.get("category") == "harness":
                saw_harness = True
        if require_harness and not saw_harness:
            finish("fail", f"{kind} {name} test log lacks required harness signal")

    if structured:
        parse_jsonl("test_log_jsonl", require_harness=True)
        parse_jsonl("artifact_index_jsonl", require_harness=False)


lib_embedded = summary.get("lib")
if not isinstance(lib_embedded, dict):
    finish("fail", "summary.lib must contain the actual inline-lib test result")
lib_result_path = evidence_resolved / "lib" / "result.json"
lib_result, lib_result_bytes = load_object(lib_result_path, "inline-lib result")
decision_inputs.append((lib_result_path, lib_result_bytes))
if lib_result != lib_embedded:
    finish("fail", "summary.lib and lib/result.json disagree")
expected_lib_fields = required_result_fields | {"target"}
if set(lib_result) != expected_lib_fields:
    finish("fail", "inline-lib result fields do not match the canonical result contract")
if lib_result.get("schema") != "pi.e2e.result.v1" or lib_result.get("result_kind") != "lib":
    finish("fail", "inline-lib result has an invalid result contract")
if lib_result.get("target") != "lib":
    finish("fail", "inline-lib result target must be 'lib'")
if lib_result.get("correlation_id") != correlation_id:
    finish("fail", "inline-lib result correlation_id does not match the run")
if lib_result.get("timestamp") != evidence_resolved.name:
    finish("fail", "inline-lib result timestamp does not match the run")
lib_duration = lib_result.get("duration_ms")
if isinstance(lib_duration, bool) or not isinstance(lib_duration, int) or lib_duration < 0:
    finish("fail", "inline-lib result duration_ms must be a non-negative integer")
validate_result_diagnostics(
    lib_result,
    "inline-lib result",
    "lib",
    evidence_resolved / "lib",
    structured=False,
)
lib_exit = lib_result.get("exit_code")
if isinstance(lib_exit, bool) or not isinstance(lib_exit, int) or lib_exit != 0:
    finish("fail", "inline-lib tests did not exit successfully")
lib_passed = uint(lib_result.get("passed"), "lib.passed")
lib_failed = uint(lib_result.get("failed"), "lib.failed")
lib_ignored = uint(lib_result.get("ignored"), "lib.ignored")
lib_total = uint(lib_result.get("total"), "lib.total")
if lib_total == 0 or lib_passed == 0:
    finish("fail", "inline-lib result executed zero passing tests")
if lib_passed + lib_failed + lib_ignored != lib_total or lib_failed != 0:
    finish("fail", "inline-lib result counts are inconsistent or failing")

for index, embedded_result in enumerate(unit_results):
    if not isinstance(embedded_result, dict):
        finish("fail", f"summary.unit_targets[{index}] must be an object")
    target_name = embedded_result.get("target")
    if not isinstance(target_name, str) or re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]*", target_name) is None:
        finish("fail", f"summary.unit_targets[{index}] has an unsafe target name")
    observed_unit_names.append(target_name)
    result_path = evidence_resolved / "unit" / target_name / "result.json"
    actual_result, result_bytes = load_object(result_path, f"integration result for {target_name}")
    decision_inputs.append((result_path, result_bytes))
    if actual_result != embedded_result:
        finish("fail", f"summary and result.json disagree for integration target {target_name}")
    if actual_result.get("schema") != "pi.e2e.result.v1" or actual_result.get("result_kind") != "unit":
        finish("fail", f"integration target {target_name} has an invalid result contract")
    missing_fields = sorted(
        (required_result_fields | structured_result_fields | {"target"}) - set(actual_result)
    )
    if missing_fields:
        finish("fail", f"integration target {target_name} result is missing fields: {missing_fields}")
    if actual_result.get("correlation_id") != correlation_id:
        finish("fail", f"integration target {target_name} correlation_id does not match the run")
    if actual_result.get("timestamp") != evidence_resolved.name:
        finish("fail", f"integration target {target_name} timestamp does not match the run")
    duration_ms = actual_result.get("duration_ms")
    if isinstance(duration_ms, bool) or not isinstance(duration_ms, int) or duration_ms < 0:
        finish("fail", f"integration target {target_name} duration_ms must be a non-negative integer")
    validate_result_diagnostics(
        actual_result,
        "integration target",
        target_name,
        evidence_resolved / "unit" / target_name,
        structured=True,
    )
    exit_code = embedded_result.get("exit_code")
    if isinstance(exit_code, bool) or not isinstance(exit_code, int) or exit_code != 0:
        finish("fail", f"integration-test target {target_name} did not exit successfully")
    passed = uint(embedded_result.get("passed"), f"{target_name}.passed")
    failed = uint(embedded_result.get("failed"), f"{target_name}.failed")
    ignored = uint(embedded_result.get("ignored"), f"{target_name}.ignored")
    total = uint(embedded_result.get("total"), f"{target_name}.total")
    if total == 0 or passed == 0:
        finish("fail", f"integration-test target {target_name} executed zero passing tests")
    if passed + failed + ignored != total or failed != 0:
        finish("fail", f"integration-test target {target_name} counts are inconsistent or failing")
if observed_unit_names != environment_units or len(observed_unit_names) != len(set(observed_unit_names)):
    finish("fail", "environment and summary integration-target identities/order do not match")

environment_suites = string_list(environment.get("e2e_suites"), "environment.e2e_suites", nonempty=True)
summary_suites = summary.get("suites")
if not isinstance(summary_suites, list) or not summary_suites:
    finish("fail", "summary.suites must contain at least one actual E2E result")
total_suites = uint(summary.get("total_suites"), "summary.total_suites")
passed_suites = uint(summary.get("passed_suites"), "summary.passed_suites")
failed_suites = uint(summary.get("failed_suites"), "summary.failed_suites")
if total_suites != len(summary_suites) or total_suites != len(environment_suites):
    finish("fail", "summary E2E totals do not match selected/result suite counts")
if passed_suites + failed_suites != total_suites or failed_suites != 0 or passed_suites != total_suites:
    finish("fail", "release evidence contains failed or unaccounted E2E suites")
if summary.get("failed_names") != []:
    finish("fail", "summary.failed_names must be an empty array")

observed_suite_names = []
for index, embedded_result in enumerate(summary_suites):
    if not isinstance(embedded_result, dict):
        finish("fail", f"summary.suites[{index}] must be an object")
    suite_name = embedded_result.get("suite")
    if not isinstance(suite_name, str) or re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]*", suite_name) is None:
        finish("fail", f"summary.suites[{index}] has an unsafe suite name")
    observed_suite_names.append(suite_name)
    result_path = evidence_resolved / suite_name / "result.json"
    actual_result, result_bytes = load_object(result_path, f"E2E result for {suite_name}")
    decision_inputs.append((result_path, result_bytes))
    if actual_result != embedded_result:
        finish("fail", f"summary and result.json disagree for E2E suite {suite_name}")
    if actual_result.get("schema") != "pi.e2e.result.v1" or actual_result.get("result_kind") != "suite":
        finish("fail", f"E2E suite {suite_name} has an invalid result contract")
    missing_fields = sorted(
        (required_result_fields | structured_result_fields | {"suite"}) - set(actual_result)
    )
    if missing_fields:
        finish("fail", f"E2E suite {suite_name} result is missing fields: {missing_fields}")
    if actual_result.get("correlation_id") != correlation_id:
        finish("fail", f"E2E suite {suite_name} correlation_id does not match the run")
    if actual_result.get("timestamp") != evidence_resolved.name:
        finish("fail", f"E2E suite {suite_name} timestamp does not match the run")
    duration_ms = actual_result.get("duration_ms")
    if isinstance(duration_ms, bool) or not isinstance(duration_ms, int) or duration_ms < 0:
        finish("fail", f"E2E suite {suite_name} duration_ms must be a non-negative integer")
    validate_result_diagnostics(
        actual_result,
        "E2E suite",
        suite_name,
        evidence_resolved / suite_name,
        structured=True,
    )
    exit_code = actual_result.get("exit_code")
    if isinstance(exit_code, bool) or not isinstance(exit_code, int) or exit_code != 0:
        finish("fail", f"E2E suite {suite_name} did not exit successfully")
    passed = uint(actual_result.get("passed"), f"{suite_name}.passed")
    failed = uint(actual_result.get("failed"), f"{suite_name}.failed")
    ignored = uint(actual_result.get("ignored"), f"{suite_name}.ignored")
    total = uint(actual_result.get("total"), f"{suite_name}.total")
    if total == 0 or passed == 0:
        finish("fail", f"E2E suite {suite_name} executed zero passing tests")
    if passed + failed + ignored != total or failed != 0:
        finish("fail", f"E2E suite {suite_name} test counts are inconsistent or failing")
if observed_suite_names != environment_suites or len(observed_suite_names) != len(set(observed_suite_names)):
    finish("fail", "environment and summary E2E suite identities/order do not match")

source_commit_values = (
    contract.get("source_commit"),
    environment.get("source_commit"),
    summary.get("source_commit"),
    environment.get("git_sha"),
)
source_snapshot_values = (
    contract.get("source_snapshot"),
    environment.get("source_snapshot"),
    summary.get("source_snapshot"),
)
if not all(
    isinstance(value, str)
    and re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", value) is not None
    for value in source_commit_values
):
    finish("fail", "E2E contract, environment, summary, and git_sha must contain canonical source commits")
if len(set(source_commit_values)) != 1:
    finish("fail", "E2E contract, environment, summary, and git_sha source commits do not match")
if not all(
    isinstance(value, str) and re.fullmatch(r"sha256:[0-9a-f]{64}", value) is not None
    for value in source_snapshot_values
):
    finish("fail", "E2E contract, environment, and summary must contain canonical source snapshots")
if len(set(source_snapshot_values)) != 1:
    finish("fail", "E2E contract, environment, and summary source snapshots do not match")
source_commit = source_commit_values[0]
source_snapshot = source_snapshot_values[0]
source_check = run_git("rev-parse", "--verify", f"{source_commit}^{{commit}}")
if source_check.returncode != 0 or source_check.stdout.strip() != source_commit:
    finish("fail", f"E2E source_commit does not resolve exactly to a commit: {source_commit}")
recomputed_source_snapshot = recompute_e2e_source_snapshot(source_commit)
if recomputed_source_snapshot != source_snapshot:
    finish(
        "fail",
        "E2E source_snapshot does not match the independently recomputed source commit: "
        f"recorded={source_snapshot} recomputed={recomputed_source_snapshot}",
    )

runner_outcome_path = evidence_resolved / "runner_outcome.json"
runner_outcome, runner_outcome_bytes = load_object(runner_outcome_path, "runner outcome")
decision_inputs.append((runner_outcome_path, runner_outcome_bytes))
runner_outcome_fields = {
    "schema",
    "generated_at",
    "timestamp",
    "profile",
    "artifact_dir",
    "correlation_id",
    "source_commit",
    "source_snapshot",
    "status",
    "exit_code",
    "source_snapshot_verified",
    "failed_phases",
}
if set(runner_outcome) != runner_outcome_fields:
    finish("fail", "runner outcome fields do not match the canonical contract")
if runner_outcome.get("schema") != "pi.e2e.runner_outcome.v1":
    finish("fail", "runner outcome has an unsupported schema")
if runner_outcome.get("generated_at") != generated_at_raw:
    finish("fail", "runner outcome generated_at does not match the run")
if runner_outcome.get("timestamp") != evidence_resolved.name:
    finish("fail", "runner outcome timestamp does not match the run")
if runner_outcome.get("profile") != profile:
    finish("fail", "runner outcome profile does not match the run")
if runner_outcome.get("artifact_dir") != artifact_dir:
    finish("fail", "runner outcome artifact_dir does not match the run")
if runner_outcome.get("correlation_id") != correlation_id:
    finish("fail", "runner outcome correlation_id does not match the run")
if runner_outcome.get("source_commit") != source_commit:
    finish("fail", "runner outcome source_commit does not match the run")
if runner_outcome.get("source_snapshot") != source_snapshot:
    finish("fail", "runner outcome source_snapshot does not match the run")
runner_exit_code = runner_outcome.get("exit_code")
if isinstance(runner_exit_code, bool) or not isinstance(runner_exit_code, int) or runner_exit_code != 0:
    finish("fail", "runner outcome exit_code must be zero")
if runner_outcome.get("status") != "pass":
    finish("fail", "runner outcome status must be pass")
if runner_outcome.get("source_snapshot_verified") is not True:
    finish("fail", "runner outcome must prove a matching final source snapshot")
if runner_outcome.get("failed_phases") != []:
    finish("fail", "runner outcome must contain no failed phases")
if summary.get("runner_outcome") != runner_outcome:
    finish("fail", "summary runner_outcome does not match runner_outcome.json")
contract_runner = contract.get("runner_outcome")
expected_contract_runner = {
    "schema": "pi.e2e.runner_outcome.v1",
    "path": str(runner_outcome_path),
    "status": "pass",
    "exit_code": 0,
}
if contract_runner != expected_contract_runner:
    finish("fail", "evidence contract runner_outcome metadata is inconsistent")

classification = run_git("show", f"{source_commit}:tests/suite_classification.toml")
if classification.returncode != 0:
    finish("fail", "unable to load the source test-suite classification")
try:
    classification_document = tomllib.loads(classification.stdout)
except (UnicodeError, tomllib.TOMLDecodeError) as exc:
    finish("fail", f"unable to parse the source test-suite classification: {exc}")
suite_table = classification_document.get("suite")
if not isinstance(suite_table, dict):
    finish("fail", "source test-suite classification has no suite table")

def classified_files(name):
    section = suite_table.get(name)
    values = section.get("files") if isinstance(section, dict) else None
    if not isinstance(values, list) or not values:
        finish("fail", f"source test-suite classification {name} list must be non-empty")
    if any(not isinstance(value, str) or re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]*", value) is None for value in values):
        finish("fail", f"source test-suite classification {name} list is invalid")
    if len(values) != len(set(values)):
        finish("fail", f"source test-suite classification {name} list contains duplicates")
    return values

classified_units = classified_files("unit")
classified_vcr = classified_files("vcr")
classified_e2e = classified_files("e2e")
classified_sets = [set(classified_units), set(classified_vcr), set(classified_e2e)]
if any(
    classified_sets[left] & classified_sets[right]
    for left in range(len(classified_sets))
    for right in range(left + 1, len(classified_sets))
):
    finish("fail", "source test-suite classifications must be pairwise disjoint")
if "e2e_extension_registration" not in classified_sets[2]:
    finish("fail", "source test-suite classification does not place the CI smoke suite in e2e")
test_tree = run_git(
    "ls-tree",
    "-r",
    "-z",
    "--name-only",
    source_commit,
    "--",
    "tests",
    text=False,
)
if test_tree.returncode != 0:
    finish("fail", "unable to enumerate source integration-test files")
tracked_test_stems = []
for path in (item for item in test_tree.stdout.split(b"\0") if item):
    try:
        decoded = path.decode("utf-8", "strict")
    except UnicodeError:
        finish("fail", "source integration-test path is not UTF-8")
    match = re.fullmatch(r"tests/([^/]+)\.rs", decoded)
    if match is not None:
        tracked_test_stems.append(match.group(1))
if len(tracked_test_stems) != len(set(tracked_test_stems)):
    finish("fail", "source tree contains duplicate top-level integration-test stems")
classified_union = classified_sets[0] | classified_sets[1] | classified_sets[2]
if classified_union != set(tracked_test_stems):
    missing = sorted(set(tracked_test_stems) - classified_union)
    nonexistent = sorted(classified_union - set(tracked_test_stems))
    finish(
        "fail",
        f"source test-suite classification must cover every tracked top-level tests/*.rs exactly once; "
        f"missing={missing[:5]} nonexistent={nonexistent[:5]}",
    )
expected_units = sorted(classified_sets[0] | classified_sets[1])
expected_suites = sorted(classified_e2e) if profile == "full" else ["e2e_extension_registration"]
if observed_unit_names != expected_units:
    finish("fail", f"profile={profile} integration scope does not match source classification")
if observed_suite_names != expected_suites:
    finish("fail", f"profile={profile} E2E scope does not match source classification")

required_check_paths = {
    "run.source_commit_format": contract_path,
    "run.source_snapshot_format": contract_path,
    "contract.source_commit_matches_run": contract_path,
    "contract.source_snapshot_matches_run": contract_path,
    "environment": environment_path,
    "environment.json_parse": environment_path,
    "environment.keys": environment_path,
    "environment.schema": environment_path,
    "environment.correlation_id_nonempty": environment_path,
    "environment.generated_at_matches_run": environment_path,
    "environment.source_commit_format": environment_path,
    "environment.source_snapshot_format": environment_path,
    "environment.source_commit_matches_run": environment_path,
    "environment.source_snapshot_matches_run": environment_path,
    "environment.git_sha_matches_source_commit": environment_path,
    "summary": summary_path,
    "summary.json_parse": summary_path,
    "summary.keys": summary_path,
    "summary.schema": summary_path,
    "summary.correlation_id_nonempty": summary_path,
    "summary.generated_at_matches_run": summary_path,
    "summary.source_commit_format": summary_path,
    "summary.source_snapshot_format": summary_path,
    "summary.source_commit_matches_run": summary_path,
    "summary.source_snapshot_matches_run": summary_path,
    "run.correlation_id_matches_environment": summary_path,
    "run.generated_at_matches_environment": summary_path,
    "run.source_commit_matches_environment": summary_path,
    "run.source_snapshot_matches_environment": summary_path,
    "summary.failed_suites_matches_suite_results": summary_path,
    "summary.lib_matches_result": summary_path,
    "summary.runner_outcome_matches_file": summary_path,
}
for check_id in (
    "runner_outcome",
    "runner_outcome.json_parse",
    "runner_outcome.keys",
    "runner_outcome.keys_exact",
    "runner_outcome.schema",
    "runner_outcome.generated_at_matches_run",
    "runner_outcome.timestamp_matches_run",
    "runner_outcome.profile_matches_run",
    "runner_outcome.artifact_dir_matches_run",
    "runner_outcome.correlation_id_matches_run",
    "runner_outcome.source_commit_matches_run",
    "runner_outcome.source_snapshot_matches_run",
    "runner_outcome.status_pass",
    "runner_outcome.exit_code_zero",
    "runner_outcome.source_snapshot_verified",
    "runner_outcome.failed_phases_empty",
):
    required_check_paths[check_id] = runner_outcome_path

lib_result_path = evidence_resolved / "lib" / "result.json"
lib_dir = evidence_resolved / "lib"
for suffix in (
    "",
    ".json_parse",
    ".keys",
    ".schema",
    ".kind",
    ".name",
    ".correlation_id_matches_summary",
    ".exit_code_zero",
    ".duration_ms_nonnegative",
    ".counts_nonnegative",
    ".counts_consistent",
    ".tests_executed",
    ".timestamp_matches_run",
    ".diagnostic_artifacts.object",
    ".diagnostic_artifacts.keys_exact",
    ".diagnostic_artifacts.schema",
    ".log_file_nonempty",
    ".log_file_path_matches",
):
    required_check_paths[f"lib:lib:result{suffix}"] = lib_result_path
for suffix in (
    ".object",
    ".keys",
    ".path_matches",
    ".sha256_format",
    ".size_format",
):
    required_check_paths[
        f"lib:lib:result.diagnostic_artifacts.output_log{suffix}"
    ] = lib_result_path
for suffix in (".regular_non_executable", ".stable_read", ".size_matches", ".sha256_matches"):
    required_check_paths[
        f"lib:lib:result.diagnostic_artifacts.output_log{suffix}"
    ] = lib_dir / "output.log"
for check_id in (
    "lib:lib:result.log_file_exists",
    "lib:lib:result.log_file_budget",
    "lib:lib:result.log_file_redaction",
):
    required_check_paths[check_id] = lib_dir / "output.log"
for field in ("test_log_jsonl", "artifact_index_jsonl"):
    required_check_paths[
        f"lib:lib:result.diagnostic_artifacts.{field}_null"
    ] = lib_result_path
for target_name in expected_units:
    result_path = evidence_resolved / "unit" / target_name / "result.json"
    for suffix in ("", ".json_parse", ".keys", ".schema", ".kind", ".name", ".correlation_id_matches_summary"):
        required_check_paths[f"unit:{target_name}:result{suffix}"] = result_path
    target_dir = evidence_resolved / "unit" / target_name
    for field, artifact_name in (
        ("log_file", "output.log"),
        ("test_log_jsonl", "test-log.jsonl"),
        ("artifact_index_jsonl", "artifact-index.jsonl"),
    ):
        required_check_paths[f"unit:{target_name}:result.{field}_nonempty"] = result_path
        required_check_paths[f"unit:{target_name}:result.{field}_path_matches"] = result_path
    for check_id in (
        f"unit:{target_name}:result.diagnostic_artifacts.object",
        f"unit:{target_name}:result.diagnostic_artifacts.schema",
    ):
        required_check_paths[check_id] = result_path
    for binding_field, artifact_name in (
        ("output_log", "output.log"),
        ("test_log_jsonl", "test-log.jsonl"),
        ("artifact_index_jsonl", "artifact-index.jsonl"),
    ):
        binding_prefix = f"unit:{target_name}:result.diagnostic_artifacts.{binding_field}"
        for suffix in ("object", "keys", "path_matches", "sha256_format", "size_format"):
            required_check_paths[f"{binding_prefix}.{suffix}"] = result_path
        for suffix in ("regular_non_executable", "stable_read", "size_matches", "sha256_matches"):
            required_check_paths[f"{binding_prefix}.{suffix}"] = target_dir / artifact_name
    for check_id in (
        f"unit:{target_name}:result.log_file_exists",
        f"unit:{target_name}:result.log_file_budget",
        f"unit:{target_name}:result.log_file_redaction",
    ):
        required_check_paths[check_id] = target_dir / "output.log"
    for check_id in (
        f"unit:{target_name}.test_log_jsonl.file_budget",
        f"unit:{target_name}.test_log_jsonl.redaction_scan",
        f"unit:{target_name}.test_log_jsonl.minimum_signal_harness_category",
    ):
        required_check_paths[check_id] = target_dir / "test-log.jsonl"
    for check_id in (
        f"unit:{target_name}.artifact_index_jsonl.file_budget",
        f"unit:{target_name}.artifact_index_jsonl.redaction_scan",
    ):
        required_check_paths[check_id] = target_dir / "artifact-index.jsonl"
for suite_name in expected_suites:
    result_path = evidence_resolved / suite_name / "result.json"
    for suffix in ("", ".json_parse", ".keys", ".schema", ".kind", ".name", ".correlation_id_matches_summary"):
        required_check_paths[f"suite:{suite_name}:result{suffix}"] = result_path
    suite_dir = evidence_resolved / suite_name
    for field, artifact_name in (
        ("log_file", "output.log"),
        ("test_log_jsonl", "test-log.jsonl"),
        ("artifact_index_jsonl", "artifact-index.jsonl"),
    ):
        required_check_paths[f"suite:{suite_name}:result.{field}_nonempty"] = result_path
        required_check_paths[f"suite:{suite_name}:result.{field}_path_matches"] = result_path
    for check_id in (
        f"suite:{suite_name}:result.diagnostic_artifacts.object",
        f"suite:{suite_name}:result.diagnostic_artifacts.schema",
    ):
        required_check_paths[check_id] = result_path
    for binding_field, artifact_name in (
        ("output_log", "output.log"),
        ("test_log_jsonl", "test-log.jsonl"),
        ("artifact_index_jsonl", "artifact-index.jsonl"),
    ):
        binding_prefix = f"suite:{suite_name}:result.diagnostic_artifacts.{binding_field}"
        for suffix in ("object", "keys", "path_matches", "sha256_format", "size_format"):
            required_check_paths[f"{binding_prefix}.{suffix}"] = result_path
        for suffix in ("regular_non_executable", "stable_read", "size_matches", "sha256_matches"):
            required_check_paths[f"{binding_prefix}.{suffix}"] = suite_dir / artifact_name
    for check_id in (
        f"suite:{suite_name}:result.log_file_exists",
        f"suite:{suite_name}:result.log_file_budget",
        f"suite:{suite_name}:result.log_file_redaction",
    ):
        required_check_paths[check_id] = suite_dir / "output.log"
    for check_id in (
        f"suite:{suite_name}.test_log_jsonl.file_budget",
        f"suite:{suite_name}.test_log_jsonl.redaction_scan",
        f"suite:{suite_name}.test_log_jsonl.minimum_signal_harness_category",
    ):
        required_check_paths[check_id] = suite_dir / "test-log.jsonl"
    for check_id in (
        f"suite:{suite_name}.artifact_index_jsonl.file_budget",
        f"suite:{suite_name}.artifact_index_jsonl.redaction_scan",
    ):
        required_check_paths[check_id] = suite_dir / "artifact-index.jsonl"
if profile == "full":
    for check_id in (
        "full_profile.rerun_mode",
        "full_profile.shard_mode",
        "full_profile.unit_scope_complete",
        "full_profile.e2e_scope_complete",
    ):
        required_check_paths[check_id] = summary_path
for check_id, expected_path in required_check_paths.items():
    check = checks_by_id.get(check_id)
    if not isinstance(check, dict) or check.get("ok") is not True:
        finish("fail", f"evidence contract is missing required passing check: {check_id}")
    try:
        recorded_path = Path(check.get("path", "")).resolve(strict=True)
        canonical_expected = expected_path.resolve(strict=True)
    except (OSError, RuntimeError) as exc:
        finish("fail", f"unable to resolve evidence-contract check path for {check_id}: {exc}")
    if recorded_path != canonical_expected:
        finish("fail", f"evidence-contract check {check_id} points at the wrong artifact")
head_check = run_git("rev-parse", "--verify", "HEAD^{commit}")
if head_check.returncode != 0:
    finish("fail", "unable to resolve current release HEAD")
current_head = head_check.stdout.strip()
ancestor_check = run_git("merge-base", "--is-ancestor", source_commit, current_head)
if ancestor_check.returncode == 1:
    finish("fail", f"E2E source commit {source_commit} is not an ancestor of release HEAD {current_head}")
if ancestor_check.returncode != 0:
    finish("fail", "unable to inspect E2E source commit ancestry")

if source_commit != current_head:
    allowed_prefixes = (
        b"docs/evidence/",
        b"tests/e2e_results/",
        b"tests/ext_conformance/reports/",
        b"tests/perf/reports/",
        b"tests/cross_platform_reports/",
        b"tests/franken_node_compat/reports/",
        b"tests/evidence_bundle/",
        b"tests/certification/",
    )
    cargo_source = run_git("show", f"{source_commit}:Cargo.toml")
    if cargo_source.returncode != 0:
        finish("fail", "unable to load source Cargo.toml package include policy")
    try:
        package_patterns = tomllib.loads(cargo_source.stdout).get("package", {}).get("include", [])
    except tomllib.TOMLDecodeError as exc:
        finish("fail", f"unable to parse source Cargo.toml package include policy: {exc}")
    if not isinstance(package_patterns, list):
        finish("fail", "source Cargo.toml package.include must be an array")

    history = run_git(
        "diff",
        "--name-only",
        "-z",
        "--no-renames",
        source_commit,
        current_head,
        text=False,
    )
    if history.returncode != 0:
        finish("fail", "unable to inspect commits following the E2E source commit")
    changed_paths = [path for path in history.stdout.split(b"\0") if path]
    disallowed = []
    for path in changed_paths:
        decoded = os.fsdecode(path)
        if not path.startswith(allowed_prefixes):
            disallowed.append(path)
        elif path.startswith(b"docs/evidence/") and package_includes(decoded, package_patterns):
            disallowed.append(path)
    if disallowed:
        examples = ", ".join(os.fsdecode(path) for path in disallowed[:5])
        finish("fail", f"non-evidence changes follow the E2E source commit: {examples}")

all_index_flags = run_git("ls-files", "-v", "-z", text=False)
if all_index_flags.returncode != 0:
    finish("fail", "unable to inspect repository index flags")
noncanonical_flags = [
    record for record in all_index_flags.stdout.split(b"\0") if record and not record.startswith(b"H ")
]
if noncanonical_flags:
    examples = ", ".join(os.fsdecode(record[2:]) for record in noncanonical_flags[:5])
    finish("fail", f"repository contains assume-unchanged/skip-worktree or non-canonical index flags: {examples}")

tracked_diff = run_git("diff", "--quiet", "HEAD", "--")
if tracked_diff.returncode == 1:
    finish("fail", "release worktree/index contains uncommitted tracked changes")
if tracked_diff.returncode != 0:
    finish("fail", "unable to inspect release worktree/index state")
untracked = run_git("ls-files", "--others", "--exclude-standard", "-z", text=False)
if untracked.returncode != 0:
    finish("fail", "unable to inspect untracked release paths")
untracked_paths = [path for path in untracked.stdout.split(b"\0") if path]
if untracked_paths:
    finish("fail", "release worktree contains untracked non-ignored paths")

for decision_path, captured_bytes in decision_inputs:
    relative = decision_path.relative_to(root_resolved).as_posix()
    tree_entry = run_git("ls-tree", "-z", "HEAD", "--", relative, text=False)
    if tree_entry.returncode != 0:
        finish("fail", f"unable to inspect committed E2E decision input: {relative}")
    record = tree_entry.stdout.removesuffix(b"\0")
    try:
        metadata, recorded_path = record.split(b"\t", 1)
        mode, object_type, object_id = metadata.split(b" ", 2)
    except ValueError:
        finish("fail", f"E2E decision input is not tracked by release HEAD: {relative}")
    if mode != b"100644" or object_type != b"blob" or os.fsdecode(recorded_path) != relative:
        finish("fail", f"E2E decision input must be a committed regular JSON blob: {relative}")

    index_entry = run_git("ls-files", "--stage", "-z", "--", relative, text=False)
    if index_entry.returncode != 0:
        finish("fail", f"unable to inspect E2E decision input index entry: {relative}")
    index_records = [item for item in index_entry.stdout.split(b"\0") if item]
    if len(index_records) != 1:
        finish("fail", f"E2E decision input must have exactly one canonical index entry: {relative}")
    try:
        index_metadata, index_path = index_records[0].split(b"\t", 1)
        index_mode, index_object_id, index_stage = index_metadata.split(b" ", 2)
    except ValueError:
        finish("fail", f"E2E decision input has a malformed index entry: {relative}")
    if (
        index_mode != mode
        or index_object_id != object_id
        or index_stage != b"0"
        or os.fsdecode(index_path) != relative
    ):
        finish("fail", f"E2E decision input index entry differs from release HEAD: {relative}")

    index_flags = run_git("ls-files", "-v", "-z", "--", relative, text=False)
    if index_flags.returncode != 0:
        finish("fail", f"unable to inspect E2E decision input index flags: {relative}")
    flag_records = [item for item in index_flags.stdout.split(b"\0") if item]
    if len(flag_records) != 1 or flag_records[0] != b"H " + relative.encode("utf-8"):
        finish("fail", f"E2E decision input has assume-unchanged/skip-worktree or non-canonical flags: {relative}")

    committed = run_git("cat-file", "blob", os.fsdecode(object_id), text=False)
    if committed.returncode != 0:
        finish("fail", f"unable to read committed E2E decision input: {relative}")
    try:
        worktree_bytes = decision_path.read_bytes()
    except OSError as exc:
        finish("fail", f"unable to read E2E decision input {relative}: {exc}")
    if committed.stdout != captured_bytes:
        finish("fail", f"E2E decision input bytes parsed by the validator differ from release HEAD: {relative}")
    if worktree_bytes != captured_bytes:
        finish("fail", f"E2E decision input changed while it was being validated: {relative}")
    if committed.stdout != worktree_bytes:
        finish("fail", f"E2E decision input bytes differ from release HEAD: {relative}")
    diff = run_git("diff", "--quiet", "HEAD", "--", relative)
    if diff.returncode == 1:
        finish("fail", f"E2E decision input index/worktree differs from release HEAD: {relative}")
    if diff.returncode != 0:
        finish("fail", f"unable to inspect E2E decision input state: {relative}")

final_head_check = run_git("rev-parse", "--verify", "HEAD^{commit}")
if final_head_check.returncode != 0 or final_head_check.stdout.strip() != current_head:
    finish("fail", "release HEAD changed while E2E decision inputs were being validated")
final_tracked_diff = run_git("diff", "--quiet", "HEAD", "--")
if final_tracked_diff.returncode == 1:
    finish("fail", "release worktree/index changed while E2E decision inputs were being validated")
if final_tracked_diff.returncode != 0:
    finish("fail", "unable to re-inspect release worktree/index state")
final_untracked = run_git("ls-files", "--others", "--exclude-standard", "-z", text=False)
if final_untracked.returncode != 0:
    finish("fail", "unable to re-inspect untracked release paths")
if any(final_untracked.stdout.split(b"\0")):
    finish("fail", "release worktree gained untracked non-ignored paths during validation")
final_index_flags = run_git("ls-files", "-v", "-z", text=False)
if final_index_flags.returncode != 0:
    finish("fail", "unable to re-inspect repository index flags")
if any(record and not record.startswith(b"H ") for record in final_index_flags.stdout.split(b"\0")):
    finish("fail", "repository index flags changed while E2E decision inputs were being validated")
for decision_path, captured_bytes in decision_inputs:
    relative = decision_path.relative_to(root_resolved).as_posix()
    try:
        metadata = decision_path.lstat()
        final_bytes = decision_path.read_bytes()
    except OSError as exc:
        finish("fail", f"unable to re-inspect E2E decision input {relative}: {exc}")
    if not stat.S_ISREG(metadata.st_mode):
        finish("fail", f"E2E decision input became a symlink or special file: {relative}")
    if os.name != "nt" and metadata.st_mode & 0o111:
        finish("fail", f"E2E decision input became executable during validation: {relative}")
    if final_bytes != captured_bytes:
        finish("fail", f"E2E decision input changed while it was being validated: {relative}")
final_head_check = run_git("rev-parse", "--verify", "HEAD^{commit}")
if final_head_check.returncode != 0 or final_head_check.stdout.strip() != current_head:
    finish("fail", "release HEAD changed during the final E2E evidence recheck")

finish(
    "pass",
    f"profile={profile}; {passed_checks}/{len(checks)} contract checks pass; "
    f"{passed_units}/{total_units} integration targets pass; {passed_suites}/{total_suites} E2E suites pass; "
    f"source_commit={source_commit}",
)
PY
    ); then
        :
    else
        EVIDENCE_CHECK="fail|unexpected E2E evidence validator error: $EVIDENCE_CHECK"
    fi
    EVIDENCE_STATUS="${EVIDENCE_CHECK%%|*}"
    EVIDENCE_DETAIL="${EVIDENCE_CHECK#*|}"
    if [[ "$EVIDENCE_STATUS" == "pass" ]]; then
        check_pass "evidence_contract" "$EVIDENCE_DETAIL"
    else
        check_fail "evidence_contract" "$EVIDENCE_DETAIL"
    fi
else
    check_fail "evidence_contract" "evidence_contract.json not found"
fi

# Gate 3: Conformance summary
CONFORMANCE_DIR="$PROJECT_ROOT/tests/ext_conformance/reports"
CONFORMANCE_SUMMARY="$CONFORMANCE_DIR/conformance_summary.json"
if [[ -f "$CONFORMANCE_SUMMARY" ]]; then
    if SUMMARY_DATA=$(python3 - "$PROJECT_ROOT" "$CONFORMANCE_SUMMARY" "$MIN_PASS_RATE" "$MAX_EVIDENCE_AGE_HOURS" 2>&1 <<'PY'
import fnmatch
import hashlib
import json
import math
import os
import re
import stat
import subprocess
import sys
import tomllib
from datetime import datetime, timedelta, timezone
from pathlib import Path

raw_root = Path(sys.argv[1])
summary_path = Path(sys.argv[2])
minimum_rate = int(sys.argv[3])
maximum_age = timedelta(hours=int(sys.argv[4]))

def reject_duplicate_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object key: {key}")
        value[key] = item
    return value


def resolve_repository_context():
    if raw_root.is_symlink() or not raw_root.is_dir():
        raise ValueError("conformance repository root must be a real directory, not a symlink")
    try:
        resolved_root = raw_root.resolve(strict=True)
        git_marker = resolved_root / ".git"
        marker_metadata = git_marker.lstat()
        if stat.S_ISLNK(marker_metadata.st_mode):
            raise ValueError("conformance repository .git marker must not be a symlink")
        if stat.S_ISDIR(marker_metadata.st_mode):
            resolved_git_dir = git_marker.resolve(strict=True)
        elif stat.S_ISREG(marker_metadata.st_mode):
            marker = git_marker.read_text(encoding="utf-8").rstrip("\r\n")
            if "\n" in marker or "\r" in marker or not marker.startswith("gitdir: "):
                raise ValueError("conformance repository .git file is malformed")
            target = Path(marker.removeprefix("gitdir: "))
            candidate = target if target.is_absolute() else resolved_root / target
            target_metadata = candidate.lstat()
            if stat.S_ISLNK(target_metadata.st_mode) or not stat.S_ISDIR(target_metadata.st_mode):
                raise ValueError(
                    "conformance repository gitfile target must be a non-symlink directory"
                )
            resolved_git_dir = candidate.resolve(strict=True)
        else:
            raise ValueError("conformance repository .git marker is not a directory or gitfile")
    except (OSError, RuntimeError, UnicodeError) as exc:
        raise ValueError(
            f"conformance repository Git context could not be resolved safely: {exc}"
        ) from exc
    if not resolved_git_dir.is_dir():
        raise ValueError("conformance repository Git directory is not a directory")
    return resolved_root, resolved_git_dir


def git_result(*args):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_LITERAL_PATHSPECS"] = "1"
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    env["GIT_OPTIONAL_LOCKS"] = "0"
    env["GIT_TERMINAL_PROMPT"] = "0"
    return subprocess.run(
        [
            "git",
            "--git-dir", str(git_dir),
            "--work-tree", str(root),
            "-c", "core.bare=false",
            "-c", "core.fsmonitor=false",
            "-c", f"core.worktree={root}",
            *args,
        ],
        capture_output=True,
        env=env,
        check=False,
    )


def git(*args):
    result = git_result(*args)
    if result.returncode != 0:
        diagnostic = result.stderr.decode("utf-8", "replace").strip()
        raise ValueError(f"git {' '.join(args)} failed: {diagnostic}")
    return result.stdout


root, git_dir = resolve_repository_context()


def verify_repository_binding():
    bindings = (
        (("rev-parse", "--show-toplevel"), root, "worktree"),
        (("rev-parse", "--absolute-git-dir"), git_dir, "Git directory"),
    )
    for args, expected, label in bindings:
        output = git(*args).decode("utf-8", "strict")
        if not output.endswith("\n") or "\n" in output.removesuffix("\n"):
            raise ValueError(f"conformance repository {label} output is not one canonical line")
        try:
            reported = Path(output.removesuffix("\n")).resolve(strict=True)
        except (OSError, RuntimeError) as exc:
            raise ValueError(
                f"unable to canonicalize conformance repository {label}: {exc}"
            ) from exc
        if reported != expected:
            raise ValueError(
                f"conformance repository {label} does not match the filesystem-derived binding"
            )


verify_repository_binding()


def canonical_lineage(value, label):
    if not isinstance(value, str) or re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:/-]{0,255}", value) is None:
        raise ValueError(f"{label} must be a non-empty canonical lineage identifier")
    return value


def package_includes(path, patterns):
    for raw_pattern in patterns:
        if not isinstance(raw_pattern, str) or not raw_pattern:
            raise ValueError("package.include entries must be non-empty strings")
        pattern = raw_pattern.removeprefix("/")
        if fnmatch.fnmatchcase(path, pattern):
            return True
        if pattern.endswith("/**") and path.startswith(pattern[:-3].rstrip("/") + "/"):
            return True
    return False


summary_relative = "tests/ext_conformance/reports/conformance_summary.json"
expected_summary_path = root / summary_relative
try:
    summary_metadata = summary_path.lstat()
    if summary_path.resolve(strict=True) != expected_summary_path.resolve(strict=True):
        raise ValueError("conformance summary path does not identify the canonical report")
except (OSError, RuntimeError) as exc:
    raise ValueError(f"unable to resolve the conformance summary path: {exc}") from exc
if not stat.S_ISREG(summary_metadata.st_mode) or (
    os.name != "nt" and summary_metadata.st_mode & 0o111
):
    raise ValueError("conformance summary must be a non-executable regular file")
current = root
for part in Path(summary_relative).parts[:-1]:
    current /= part
    metadata = os.lstat(current)
    if stat.S_ISLNK(metadata.st_mode):
        raise ValueError(f"conformance summary traverses symlinked parent: {current}")

head = git("rev-parse", "--verify", "HEAD^{commit}").decode("ascii", "strict").strip()
head_tracked_paths = {
    os.fsdecode(path)
    for path in git("ls-tree", "-r", "-z", "--name-only", "HEAD").split(b"\0")
    if path
}
head_entry = [record for record in git("ls-tree", "-z", "HEAD", "--", summary_relative).split(b"\0") if record]
if len(head_entry) != 1:
    raise ValueError("conformance summary must be a single tracked blob in release HEAD")
try:
    metadata, recorded_path = head_entry[0].split(b"\t", 1)
    mode, object_type, head_oid = metadata.split(b" ", 2)
except ValueError as exc:
    raise ValueError("malformed conformance summary tree entry") from exc
if mode != b"100644" or object_type != b"blob" or os.fsdecode(recorded_path) != summary_relative:
    raise ValueError("conformance summary must be a canonical non-executable file blob in release HEAD")

index_entry = [record for record in git("ls-files", "--stage", "-z", "--", summary_relative).split(b"\0") if record]
if len(index_entry) != 1:
    raise ValueError("conformance summary must have one index entry")
index_metadata, index_path = index_entry[0].split(b"\t", 1)
index_mode, index_oid, index_stage = index_metadata.split(b" ", 2)
if (
    index_mode != mode
    or index_oid != head_oid
    or index_stage != b"0"
    or os.fsdecode(index_path) != summary_relative
):
    raise ValueError("conformance summary index entry differs from release HEAD")
flags = [record for record in git("ls-files", "-v", "-z", "--", summary_relative).split(b"\0") if record]
if flags != [b"H " + os.fsencode(summary_relative)]:
    raise ValueError("conformance summary uses non-canonical index flags")

raw_summary = summary_path.read_bytes()
framed = b"blob " + str(len(raw_summary)).encode("ascii") + b"\0" + raw_summary
if len(head_oid) == 40:
    worktree_oid = hashlib.sha1(framed).hexdigest().encode("ascii")
elif len(head_oid) == 64:
    worktree_oid = hashlib.sha256(framed).hexdigest().encode("ascii")
else:
    raise ValueError("unsupported Git object ID length for conformance summary")
if worktree_oid != head_oid:
    raise ValueError("raw conformance summary bytes differ from release HEAD")

def capture_head_bound_file(relative, label):
    path = root / relative
    try:
        file_metadata = path.lstat()
    except OSError as exc:
        raise ValueError(f"unable to inspect {label}: {exc}") from exc
    if not stat.S_ISREG(file_metadata.st_mode) or (
        os.name != "nt" and file_metadata.st_mode & 0o111
    ):
        raise ValueError(f"{label} must be a non-executable regular file")
    current = root
    for part in Path(relative).parts[:-1]:
        current /= part
        metadata = os.lstat(current)
        if stat.S_ISLNK(metadata.st_mode):
            raise ValueError(f"{label} traverses symlinked parent: {current}")
    tree_records = [
        record for record in git("ls-tree", "-z", "HEAD", "--", relative).split(b"\0") if record
    ]
    if len(tree_records) != 1:
        raise ValueError(f"{label} must be one tracked blob in release HEAD")
    try:
        tree_metadata, tree_path = tree_records[0].split(b"\t", 1)
        tree_mode, tree_type, tree_oid = tree_metadata.split(b" ", 2)
    except ValueError as exc:
        raise ValueError(f"malformed {label} tree entry") from exc
    if tree_mode != b"100644" or tree_type != b"blob" or os.fsdecode(tree_path) != relative:
        raise ValueError(f"{label} must be a canonical non-executable blob")
    index_records = [
        record for record in git("ls-files", "--stage", "-z", "--", relative).split(b"\0") if record
    ]
    if len(index_records) != 1:
        raise ValueError(f"{label} must have one index entry")
    try:
        index_metadata, index_path = index_records[0].split(b"\t", 1)
        index_mode, index_oid, index_stage = index_metadata.split(b" ", 2)
    except ValueError as exc:
        raise ValueError(f"malformed {label} index entry") from exc
    if (
        index_mode != tree_mode
        or index_oid != tree_oid
        or index_stage != b"0"
        or os.fsdecode(index_path) != relative
    ):
        raise ValueError(f"{label} index entry differs from release HEAD")
    flag_records = [
        record for record in git("ls-files", "-v", "-z", "--", relative).split(b"\0") if record
    ]
    if flag_records != [b"H " + os.fsencode(relative)]:
        raise ValueError(f"{label} uses non-canonical index flags")
    raw = path.read_bytes()
    committed = git("cat-file", "blob", os.fsdecode(tree_oid))
    if raw != committed:
        raise ValueError(f"{label} bytes differ from release HEAD")
    return path, raw, tree_records, index_records, flag_records


decision_inputs = {}
absent_decision_paths = set()


def capture_decision_input(relative, label):
    previous = decision_inputs.get(relative)
    if previous is not None:
        return previous[2]
    captured = capture_head_bound_file(relative, label)
    decision_inputs[relative] = (label, *captured)
    return captured[1]


def decision_source_present(relative, label):
    worktree_present = os.path.lexists(root / relative)
    if relative in head_tracked_paths or worktree_present:
        capture_decision_input(relative, label)
        return True
    absent_decision_paths.add(relative)
    return False


def parse_json_document(raw, label):
    try:
        value = json.loads(raw.decode("utf-8", "strict"), object_pairs_hook=reject_duplicate_keys)
    except (UnicodeError, json.JSONDecodeError, ValueError) as exc:
        raise ValueError(f"unable to parse {label}: {exc}") from exc
    if not isinstance(value, dict):
        raise ValueError(f"{label} root must be an object")
    return value


def parse_jsonl_document(raw, label, *, nonempty=True):
    if not raw:
        if nonempty:
            raise ValueError(f"{label} must be non-empty JSONL")
        return []
    if not raw.endswith(b"\n"):
        raise ValueError(f"{label} must be newline-terminated JSONL")
    lines = raw.removesuffix(b"\n").split(b"\n")
    values = []
    for index, line in enumerate(lines, start=1):
        if not line:
            raise ValueError(f"{label} contains an empty record at line {index}")
        try:
            value = json.loads(
                line.decode("utf-8", "strict"), object_pairs_hook=reject_duplicate_keys
            )
        except (UnicodeError, json.JSONDecodeError, ValueError) as exc:
            raise ValueError(f"unable to parse {label} line {index}: {exc}") from exc
        if not isinstance(value, dict):
            raise ValueError(f"{label} line {index} must be an object")
        values.append(value)
    return values


def uint(value, label, *, nullable=False):
    if nullable and value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= 2**63 - 1:
        suffix = " or null" if nullable else ""
        raise ValueError(f"{label} must be a non-negative signed 64-bit integer{suffix}")
    return value


def finite_number(value, label, *, nullable=False):
    if nullable and value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        suffix = " or null" if nullable else ""
        raise ValueError(f"{label} must be a finite number{suffix}")
    return float(value)


def canonical_timestamp(value, label, *, millis):
    precision = r"\.\d{3}" if millis else ""
    if not isinstance(value, str) or re.fullmatch(
        rf"\d{{4}}-\d{{2}}-\d{{2}}T\d{{2}}:\d{{2}}:\d{{2}}{precision}Z", value
    ) is None:
        expected = "UTC millisecond precision" if millis else "UTC second precision"
        raise ValueError(f"{label} must use canonical {expected}")
    try:
        return datetime.fromisoformat(value.removesuffix("Z") + "+00:00")
    except ValueError as exc:
        raise ValueError(f"{label} is not a valid UTC timestamp") from exc

events_relative = "tests/ext_conformance/reports/conformance_events.jsonl"
events_path, raw_events, events_head_entry, events_index_entry, events_flags = (
    capture_head_bound_file(events_relative, "conformance events")
)
decision_inputs[events_relative] = (
    "conformance events",
    events_path,
    raw_events,
    events_head_entry,
    events_index_entry,
    events_flags,
)

try:
    data = json.loads(
        raw_summary.decode("utf-8", "strict"), object_pairs_hook=reject_duplicate_keys
    )
except (UnicodeError, json.JSONDecodeError, ValueError) as exc:
    raise ValueError(f"unable to parse conformance summary: {exc}") from exc
if not isinstance(data, dict):
    raise ValueError("summary root must be an object")
if data.get("schema") != "pi.ext.conformance_summary.v2":
    raise ValueError(f"unsupported conformance summary schema: {data.get('schema')!r}")
expected_summary_fields = {
    "schema",
    "generated_at",
    "run_id",
    "correlation_id",
    "git_commit",
    "source_tree_sha256",
    "counts",
    "pass_rate_pct",
    "coverage_rate_pct",
    "negative",
    "per_tier",
    "evidence",
}
if set(data) != expected_summary_fields:
    raise ValueError("conformance summary top-level fields do not match the canonical v2 contract")

generated_at_raw = data.get("generated_at")
if not isinstance(generated_at_raw, str) or re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z", generated_at_raw) is None:
    raise ValueError("generated_at must use canonical UTC second precision")
generated_at = datetime.fromisoformat(generated_at_raw.removesuffix("Z") + "+00:00")
now = datetime.now(timezone.utc)
if generated_at > now + timedelta(minutes=5):
    raise ValueError("conformance summary timestamp is more than five minutes in the future")
if now - generated_at > maximum_age:
    raise ValueError(
        f"conformance summary is stale ({now - generated_at} old; maximum {maximum_age})"
    )
run_id = canonical_lineage(data.get("run_id"), "run_id")
correlation_id = canonical_lineage(data.get("correlation_id"), "correlation_id")

source_commit = data.get("git_commit")
if not isinstance(source_commit, str) or re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", source_commit) is None:
    raise ValueError("git_commit must be a canonical full lowercase object ID")
resolved_source = git("rev-parse", "--verify", f"{source_commit}^{{commit}}").decode("ascii", "strict").strip()
if resolved_source != source_commit:
    raise ValueError("git_commit does not resolve to the exact recorded commit")
ancestor = git_result("merge-base", "--is-ancestor", source_commit, head)
if ancestor.returncode == 1:
    raise ValueError("conformance source commit is not an ancestor of release HEAD")
if ancestor.returncode != 0:
    raise ValueError("unable to verify conformance source ancestry")

source_tree = git("ls-tree", "-r", "-z", "--full-tree", source_commit)
source_tracked_paths = set()
for record in (item for item in source_tree.split(b"\0") if item):
    try:
        source_metadata, source_path = record.split(b"\t", 1)
        _, source_object_type, _ = source_metadata.split(b" ", 2)
    except ValueError as exc:
        raise ValueError("source conformance tree contains a malformed record") from exc
    if source_object_type != b"blob":
        raise ValueError("source conformance tree contains a non-blob entry")
    source_tracked_paths.add(os.fsdecode(source_path))
expected_tree_digest = hashlib.sha256(source_tree).hexdigest()
recorded_tree_digest = data.get("source_tree_sha256")
if not isinstance(recorded_tree_digest, str) or re.fullmatch(r"[0-9a-f]{64}", recorded_tree_digest) is None:
    raise ValueError("source_tree_sha256 must be a lowercase SHA-256 digest")
if recorded_tree_digest != expected_tree_digest:
    raise ValueError("source_tree_sha256 does not match the canonical source tree byte stream")

try:
    cargo_document = tomllib.loads(git("show", f"{source_commit}:Cargo.toml").decode("utf-8", "strict"))
except (UnicodeError, tomllib.TOMLDecodeError) as exc:
    raise ValueError(f"unable to parse source Cargo.toml package include policy: {exc}") from exc
package_patterns = cargo_document.get("package", {}).get("include", [])
if not isinstance(package_patterns, list):
    raise ValueError("source Cargo.toml package.include must be an array")

changed_paths = [
    os.fsdecode(path)
    for path in git(
        "diff",
        "--name-only",
        "-z",
        "--no-renames",
        source_commit,
        head,
    ).split(b"\0")
    if path
]
for path in changed_paths:
    evidence_only = (
        path.startswith("tests/e2e_results/")
        or path.startswith("tests/ext_conformance/reports/")
        or path.startswith("tests/certification/")
        or path.startswith("docs/evidence/")
    )
    if not evidence_only:
        raise ValueError(f"non-evidence path changed after conformance source commit: {path}")
    if path.startswith("docs/evidence/") and package_includes(path, package_patterns):
        raise ValueError(f"packaged or product-consumed evidence changed after source capture: {path}")

manifest_result = git("show", f"{source_commit}:tests/ext_conformance/VALIDATED_MANIFEST.json")
try:
    manifest = json.loads(
        manifest_result.decode("utf-8", "strict"),
        object_pairs_hook=reject_duplicate_keys,
    )
except (UnicodeError, json.JSONDecodeError, ValueError) as exc:
    raise ValueError(f"unable to parse source conformance manifest: {exc}") from exc
if not isinstance(manifest, dict) or manifest.get("schema") != "pi.ext.validated-manifest.v1":
    raise ValueError("source conformance manifest has an unsupported schema")
manifest_extensions = manifest.get("extensions")
if not isinstance(manifest_extensions, list) or not manifest_extensions:
    raise ValueError("source conformance manifest extensions must be non-empty")
expected_extension_ids = []
expected_source_tiers = []
expected_extensions = {}
entry_path_to_extension_id = {}
capability_fields = {
    "registers_tools",
    "registers_commands",
    "registers_flags",
    "registers_providers",
    "subscribes_events",
    "uses_exec",
    "uses_http",
    "uses_ui",
    "uses_session",
    "is_multi_file",
    "has_npm_deps",
}
registration_fields = {"tools", "commands", "flags", "event_handlers"}


def canonical_relative_path(value, label):
    if not isinstance(value, str) or not value or "\x00" in value or "\\" in value:
        raise ValueError(f"{label} must be a non-empty relative path")
    candidate = Path(value)
    if candidate.is_absolute() or value != candidate.as_posix() or any(
        part in ("", ".", "..") for part in candidate.parts
    ):
        raise ValueError(f"{label} must be a canonical relative path")
    return value


def canonical_string_array(value, label):
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        raise ValueError(f"{label} must be an array of non-empty strings")
    if len(value) != len(set(value)):
        raise ValueError(f"{label} must not contain duplicates")
    return value


for index, extension in enumerate(manifest_extensions):
    if not isinstance(extension, dict):
        raise ValueError(f"source conformance manifest extension[{index}] must be an object")
    extension_id = extension.get("id")
    source_tier = extension.get("source_tier")
    extension_id = canonical_relative_path(
        extension_id, f"source conformance manifest extension[{index}].id"
    )
    source_tier = canonical_lineage(
        source_tier, f"source conformance manifest extension[{index}].source_tier"
    )
    entry_path = canonical_relative_path(
        extension.get("entry_path"), f"source conformance manifest extension[{index}].entry_path"
    )
    conformance_tier = uint(
        extension.get("conformance_tier"),
        f"source conformance manifest extension[{index}].conformance_tier",
    )
    if conformance_tier not in (1, 2, 3, 4, 5):
        raise ValueError(f"source conformance manifest extension[{index}].conformance_tier is invalid")
    capabilities = extension.get("capabilities")
    if not isinstance(capabilities, dict) or set(capabilities) != capability_fields:
        raise ValueError(f"source conformance manifest extension[{index}].capabilities is invalid")
    for field in capability_fields - {"subscribes_events"}:
        if not isinstance(capabilities[field], bool):
            raise ValueError(
                f"source conformance manifest extension[{index}].capabilities.{field} must be boolean"
            )
    canonical_string_array(
        capabilities["subscribes_events"],
        f"source conformance manifest extension[{index}].capabilities.subscribes_events",
    )
    registrations = extension.get("registrations")
    if not isinstance(registrations, dict) or set(registrations) != registration_fields:
        raise ValueError(f"source conformance manifest extension[{index}].registrations is invalid")
    for field in registration_fields:
        canonical_string_array(
            registrations[field],
            f"source conformance manifest extension[{index}].registrations.{field}",
        )
    expected_extension_ids.append(extension_id)
    expected_source_tiers.append(source_tier)
    if entry_path in entry_path_to_extension_id:
        raise ValueError("source conformance manifest contains duplicate entry paths")
    entry_path_to_extension_id[entry_path] = extension_id
    expected_extensions[extension_id] = {
        "source_tier": source_tier,
        "entry_path": entry_path,
        "conformance_tier": conformance_tier,
        "capabilities": capabilities,
        "registrations": registrations,
    }
if len(expected_extension_ids) != len(set(expected_extension_ids)):
    raise ValueError("source conformance manifest contains duplicate extension IDs")

provenance_versions = {}
provenance_result = git_result("show", f"{source_commit}:docs/extension-artifact-provenance.json")
if provenance_result.returncode == 0:
    provenance = parse_json_document(provenance_result.stdout, "source extension provenance")
    provenance_items = provenance.get("items")
    if not isinstance(provenance_items, list):
        raise ValueError("source extension provenance items must be an array")
    for index, item in enumerate(provenance_items):
        if not isinstance(item, dict):
            raise ValueError(f"source extension provenance item[{index}] must be an object")
        extension_id = item.get("id")
        if not isinstance(extension_id, str) or not extension_id:
            raise ValueError(f"source extension provenance item[{index}].id is invalid")
        version = item.get("version")
        if version is not None and (not isinstance(version, str) or not version.strip()):
            raise ValueError(f"source extension provenance item[{index}].version is invalid")
        if version is not None:
            if extension_id in provenance_versions:
                raise ValueError("source extension provenance contains duplicate versioned IDs")
            provenance_versions[extension_id] = version.strip()


def validate_input_timestamp(value, label, *, millis=False):
    timestamp = canonical_timestamp(value, label, millis=millis)
    if timestamp > now + timedelta(minutes=5):
        raise ValueError(f"{label} is more than five minutes in the future")
    if now - timestamp > maximum_age:
        raise ValueError(f"{label} is stale ({now - timestamp} old; maximum {maximum_age})")
    if timestamp > generated_at + timedelta(minutes=5):
        raise ValueError(f"{label} postdates the conformance summary run")
    return timestamp


def validate_rate(value, passed_count, failed_count, label):
    actual = finite_number(value, label)
    expected = 100.0 * passed_count / (passed_count + failed_count) if passed_count + failed_count else 100.0
    if not 0 <= actual <= 100 or not math.isclose(actual, expected, rel_tol=1e-9, abs_tol=1e-9):
        raise ValueError(f"{label} is inconsistent with its retained pass/fail records")


statuses = {
    extension_id: {
        "rust_load_ms": None,
        "ts_load_ms": None,
        "load_ratio": None,
        "scenario_pass": 0,
        "scenario_fail": 0,
        "scenario_skip": 0,
        "failures": [],
        "smoke_pass": 0,
        "smoke_fail": 0,
        "parity_match": 0,
        "parity_mismatch": 0,
    }
    for extension_id in expected_extension_ids
}


def require_manifest_extension(extension_id, label):
    if extension_id not in statuses:
        raise ValueError(f"{label} references extension outside the source manifest: {extension_id!r}")
    return statuses[extension_id]


load_relative = "tests/ext_conformance/reports/load_time_benchmark.json"
load_report = parse_json_document(
    capture_decision_input(load_relative, "load-time benchmark"), "load-time benchmark"
)
if load_report.get("schema") != "pi.ext.load_time_benchmark.v1":
    raise ValueError("load-time benchmark has an unsupported schema")
validate_input_timestamp(load_report.get("generated_at"), "load-time benchmark generated_at")
load_results = load_report.get("results")
if not isinstance(load_results, list):
    raise ValueError("load-time benchmark results must be an array")
load_derived_counts = {"total": len(load_results), "ts_success": 0, "rust_success": 0, "paired": 0}
seen_load_extensions = set()
for index, result in enumerate(load_results):
    if not isinstance(result, dict):
        raise ValueError(f"load-time benchmark result[{index}] must be an object")
    extension_path = result.get("extension")
    if not isinstance(extension_path, str) or not extension_path:
        raise ValueError(f"load-time benchmark result[{index}].extension is invalid")
    extension_id = entry_path_to_extension_id.get(extension_path)
    if extension_id is None:
        raise ValueError(
            f"load-time benchmark result[{index}].extension differs from the source manifest"
        )
    status = require_manifest_extension(extension_id, f"load-time benchmark result[{index}]")
    if extension_id in seen_load_extensions:
        raise ValueError(f"load-time benchmark results duplicate extension {extension_id}")
    seen_load_extensions.add(extension_id)
    runtime_values = {}
    for runtime in ("ts", "rust"):
        runtime_result = result.get(runtime)
        if not isinstance(runtime_result, dict):
            raise ValueError(f"load-time benchmark result[{index}].{runtime} must be an object")
        runtime_values[runtime] = uint(
            runtime_result.get("load_time_ms"),
            f"load-time benchmark result[{index}].{runtime}.load_time_ms",
            nullable=True,
        )
        success = runtime_result.get("success")
        error = runtime_result.get("error")
        if not isinstance(success, bool) or (error is not None and not isinstance(error, str)):
            raise ValueError(f"load-time benchmark result[{index}].{runtime} outcome is malformed")
        if success and (runtime_values[runtime] is None or error is not None):
            raise ValueError(
                f"load-time benchmark result[{index}].{runtime} success contradicts its result"
            )
        if not success and (not isinstance(error, str) or not error):
            raise ValueError(
                f"load-time benchmark result[{index}].{runtime} failure lacks an error"
            )
        if runtime == "rust" and success != (runtime_values[runtime] is not None):
            raise ValueError(
                f"load-time benchmark result[{index}].rust success contradicts load_time_ms"
            )
        load_derived_counts[f"{runtime}_success"] += int(success)
    load_derived_counts["paired"] += int(
        runtime_values["ts"] is not None
        and runtime_values["ts"] > 0
        and runtime_values["rust"] is not None
    )
    ratio = finite_number(
        result.get("ratio"), f"load-time benchmark result[{index}].ratio", nullable=True
    )
    if ratio is not None and ratio < 0:
        raise ValueError(f"load-time benchmark result[{index}].ratio must be non-negative")
    expected_ratio = (
        float(runtime_values["rust"]) / float(runtime_values["ts"])
        if runtime_values["rust"] is not None
        and runtime_values["ts"] is not None
        and runtime_values["ts"] > 0
        else None
    )
    if (ratio is None) != (expected_ratio is None) or (
        ratio is not None
        and not math.isclose(ratio, expected_ratio, rel_tol=1e-9, abs_tol=1e-9)
    ):
        raise ValueError(
            f"load-time benchmark result[{index}].ratio contradicts retained load times"
        )
    status["ts_load_ms"] = runtime_values["ts"]
    status["rust_load_ms"] = runtime_values["rust"]
    status["load_ratio"] = ratio
load_counts = load_report.get("counts")
if not isinstance(load_counts, dict) or set(load_counts) != set(load_derived_counts):
    raise ValueError("load-time benchmark counts have an invalid shape")
for field, expected in load_derived_counts.items():
    if uint(load_counts.get(field), f"load-time benchmark counts.{field}") != expected:
        raise ValueError(f"load-time benchmark counts.{field} does not match retained results")

scenario_relative = "tests/ext_conformance/reports/scenario_conformance.json"
scenario_report = parse_json_document(
    capture_decision_input(scenario_relative, "scenario conformance"), "scenario conformance"
)
if scenario_report.get("schema") != "pi.ext.scenario_conformance.v1":
    raise ValueError("scenario conformance report has an unsupported schema")
validate_input_timestamp(scenario_report.get("generated_at"), "scenario conformance generated_at")
scenario_results = scenario_report.get("results")
if not isinstance(scenario_results, list):
    raise ValueError("scenario conformance results must be an array")
scenario_counts = {"total": len(scenario_results), "pass": 0, "fail": 0, "error": 0, "skip": 0}
seen_scenarios = set()
for index, result in enumerate(scenario_results):
    if not isinstance(result, dict):
        raise ValueError(f"scenario conformance result[{index}] must be an object")
    extension_id = result.get("extension_id")
    if not isinstance(extension_id, str) or not extension_id:
        raise ValueError(f"scenario conformance result[{index}].extension_id is invalid")
    status = require_manifest_extension(extension_id, f"scenario conformance result[{index}]")
    scenario_id = result.get("scenario_id")
    if not isinstance(scenario_id, str) or not scenario_id or scenario_id in seen_scenarios:
        raise ValueError(f"scenario conformance result[{index}].scenario_id is invalid or duplicate")
    seen_scenarios.add(scenario_id)
    outcome = result.get("status")
    if outcome not in ("pass", "fail", "error", "skip"):
        raise ValueError(f"scenario conformance result[{index}].status is invalid")
    scenario_counts[outcome] += 1
    uint(result.get("duration_ms"), f"scenario conformance result[{index}].duration_ms")
    if outcome == "pass":
        status["scenario_pass"] += 1
    elif outcome in ("fail", "error"):
        status["scenario_fail"] += 1
        failure = result.get("summary")
        if not isinstance(failure, str):
            failure = result.get("error")
        if failure is not None and not isinstance(failure, str):
            raise ValueError(f"scenario conformance result[{index}] failure text is invalid")
        if isinstance(failure, str):
            status["failures"].append(failure)
    else:
        status["scenario_skip"] += 1
reported_scenario_counts = scenario_report.get("counts")
if not isinstance(reported_scenario_counts, dict) or set(reported_scenario_counts) != set(scenario_counts):
    raise ValueError("scenario conformance counts have an invalid shape")
for field, expected in scenario_counts.items():
    if uint(reported_scenario_counts.get(field), f"scenario conformance counts.{field}") != expected:
        raise ValueError(f"scenario conformance counts.{field} does not match retained results")
validate_rate(
    scenario_report.get("pass_rate_pct"),
    scenario_counts["pass"],
    scenario_counts["fail"] + scenario_counts["error"],
    "scenario conformance pass_rate_pct",
)


def validate_smoke_report(report, relative):
    if report.get("schema") != "pi.ext.smoke_triage.v1":
        raise ValueError(f"{relative} has an unsupported schema")
    timestamp = validate_input_timestamp(report.get("generated_at"), f"{relative} generated_at")
    extensions = report.get("extensions")
    if not isinstance(extensions, list):
        raise ValueError(f"{relative} extensions must be an array")
    derived = {"total": 0, "pass": 0, "fail": 0, "error": 0, "skip": 0}
    parsed = []
    for index, entry in enumerate(extensions):
        if not isinstance(entry, dict):
            raise ValueError(f"{relative} extension[{index}] must be an object")
        extension_id = entry.get("extension_id")
        if not isinstance(extension_id, str) or not extension_id:
            raise ValueError(f"{relative} extension[{index}].extension_id is invalid")
        require_manifest_extension(extension_id, f"{relative} extension[{index}]")
        counters = {}
        for field in ("pass", "fail", "error", "skip"):
            counters[field] = uint(entry.get(field), f"{relative} extension[{index}].{field}")
            derived[field] += counters[field]
        derived["total"] += sum(counters.values())
        if not isinstance(entry.get("failures"), list) or not isinstance(
            entry.get("failure_categories"), dict
        ):
            raise ValueError(f"{relative} extension[{index}] failure diagnostics are malformed")
        parsed.append((extension_id, counters))
    counts = report.get("counts")
    if not isinstance(counts, dict) or set(counts) != set(derived):
        raise ValueError(f"{relative} counts have an invalid shape")
    for field, expected in derived.items():
        if uint(counts.get(field), f"{relative} counts.{field}") != expected:
            raise ValueError(f"{relative} counts.{field} does not match retained extensions")
    validate_rate(
        report.get("pass_rate_pct"),
        derived["pass"],
        derived["fail"] + derived["error"],
        f"{relative} pass_rate_pct",
    )
    return timestamp, parsed


smoke_candidates = []
for candidate_index, relative in enumerate(
    ("tests/ext_conformance/reports/smoke_triage.json", "tests/ext_conformance/reports/smoke/triage.json")
):
    if decision_source_present(relative, relative):
        report = parse_json_document(capture_decision_input(relative, relative), relative)
        timestamp, parsed = validate_smoke_report(report, relative)
        smoke_candidates.append((timestamp, -candidate_index, parsed, relative))
if not smoke_candidates:
    raise ValueError("no tracked smoke triage report is available")
_, _, selected_smoke, selected_smoke_relative = max(smoke_candidates, key=lambda item: item[:2])
for extension_id, counters in selected_smoke:
    status = statuses[extension_id]
    status["smoke_pass"] += counters["pass"]
    status["smoke_fail"] += counters["fail"] + counters["error"]

parity_relative = "tests/ext_conformance/reports/parity/parity_events.jsonl"
parity_events = []
if decision_source_present(parity_relative, "parity events"):
    parity_events = parse_jsonl_document(
        capture_decision_input(parity_relative, "parity events"), "parity events", nonempty=False
    )
parity_main_records = {}
parity_run_ids = set()
parity_main_fields = {
    "schema",
    "run_id",
    "ts",
    "extension_id",
    "scenario_id",
    "kind",
    "summary",
    "source_tier",
    "runtime_tier",
    "status",
    "ts_ms",
    "rust_ms",
    "diffs",
    "error",
    "skip_reason",
}
for index, event in enumerate(parity_events):
    if set(event) != parity_main_fields or event.get("schema") != "pi.ext.parity.v1":
        raise ValueError(f"parity event[{index}] has an invalid canonical schema")
    extension_id = event.get("extension_id")
    if not isinstance(extension_id, str) or not extension_id:
        raise ValueError(f"parity event[{index}].extension_id is invalid")
    require_manifest_extension(extension_id, f"parity event[{index}]")
    expected_extension = expected_extensions[extension_id]
    scenario_id = event.get("scenario_id")
    if not isinstance(scenario_id, str) or not scenario_id:
        raise ValueError(f"parity event[{index}].scenario_id is invalid")
    record_key = (extension_id, scenario_id)
    if record_key in parity_main_records:
        raise ValueError(f"parity event[{index}] duplicates {extension_id}/{scenario_id}")
    parity_run_id = canonical_lineage(
        event.get("run_id"), f"parity event[{index}].run_id"
    )
    parity_run_ids.add(parity_run_id)
    for field in ("kind", "summary", "runtime_tier"):
        if not isinstance(event.get(field), str) or not event[field]:
            raise ValueError(f"parity event[{index}].{field} is invalid")
    if event.get("source_tier") != expected_extension["source_tier"]:
        raise ValueError(f"parity event[{index}].source_tier differs from the source manifest")
    uint(event.get("ts_ms"), f"parity event[{index}].ts_ms")
    uint(event.get("rust_ms"), f"parity event[{index}].rust_ms")
    outcome = event.get("status")
    if outcome not in ("match", "mismatch", "ts_error", "rust_error", "skip"):
        raise ValueError(f"parity event[{index}].status is invalid")
    validate_input_timestamp(event.get("ts"), f"parity event[{index}].ts", millis=True)
    diffs = event.get("diffs", [])
    if not isinstance(diffs, list) or any(not isinstance(item, str) for item in diffs):
        raise ValueError(f"parity event[{index}].diffs is invalid")
    error = event.get("error")
    skip_reason = event.get("skip_reason")
    if error is not None and (not isinstance(error, str) or not error):
        raise ValueError(f"parity event[{index}].error is invalid")
    if skip_reason is not None and (not isinstance(skip_reason, str) or not skip_reason):
        raise ValueError(f"parity event[{index}].skip_reason is invalid")
    if outcome == "match" and diffs:
        raise ValueError(f"parity event[{index}] reports a match with retained diffs")
    if outcome == "mismatch" and not diffs:
        raise ValueError(f"parity event[{index}] reports a mismatch without retained diffs")
    if outcome in ("ts_error", "rust_error") and error is None:
        raise ValueError(f"parity event[{index}] reports an error without diagnostics")
    if outcome == "skip" and skip_reason is None:
        raise ValueError(f"parity event[{index}] reports a skip without a reason")
    if outcome in ("match", "mismatch") and (error is not None or skip_reason is not None):
        raise ValueError(f"parity event[{index}] has diagnostics for the wrong outcome")
    if outcome in ("ts_error", "rust_error") and (diffs or skip_reason is not None):
        raise ValueError(f"parity event[{index}] has diagnostics for the wrong error outcome")
    if outcome == "skip" and (diffs or error is not None):
        raise ValueError(f"parity event[{index}] has diagnostics for the wrong skip outcome")
    parity_main_records[record_key] = event
if len(parity_run_ids) > 1:
    raise ValueError("parity events combine multiple run IDs")

negative_triage_relative = "tests/ext_conformance/reports/negative/triage.json"
negative_events_relative = "tests/ext_conformance/reports/negative/negative_events.jsonl"
negative_triage = parse_json_document(
    capture_decision_input(negative_triage_relative, "negative conformance triage"),
    "negative conformance triage",
)
if set(negative_triage) != {"schema", "generated_at", "counts", "pass_rate_pct"} or negative_triage.get(
    "schema"
) != "pi.ext.negative_triage.v1":
    raise ValueError("negative conformance triage has an invalid canonical schema")
negative_generated = validate_input_timestamp(
    negative_triage.get("generated_at"), "negative conformance triage generated_at"
)
negative_events = parse_jsonl_document(
    capture_decision_input(negative_events_relative, "negative conformance events"),
    "negative conformance events",
)
negative_derived = {"total": len(negative_events), "pass": 0, "fail": 0}
negative_event_fields = {
    "schema",
    "ts",
    "test_name",
    "capability",
    "mode",
    "reason",
    "expected_decision",
    "actual_decision",
    "status",
    "duration_ms",
}
seen_negative_tests = set()
for index, event in enumerate(negative_events):
    if set(event) != negative_event_fields or event.get("schema") != "pi.ext.negative_conformance.v1":
        raise ValueError(f"negative conformance event[{index}] has an invalid canonical schema")
    for field in ("test_name", "mode", "reason", "expected_decision", "actual_decision"):
        if not isinstance(event.get(field), str) or not event[field]:
            raise ValueError(f"negative conformance event[{index}].{field} is invalid")
    if not isinstance(event.get("capability"), str):
        raise ValueError(f"negative conformance event[{index}].capability is invalid")
    if event["mode"] not in ("strict", "prompt", "permissive"):
        raise ValueError(f"negative conformance event[{index}].mode is unsupported")
    if event["expected_decision"] not in ("allow", "deny", "prompt"):
        raise ValueError(
            f"negative conformance event[{index}].expected_decision is unsupported"
        )
    if event["actual_decision"] not in ("Allow", "Deny", "Prompt"):
        raise ValueError(
            f"negative conformance event[{index}].actual_decision is unsupported"
        )
    if event["test_name"] in seen_negative_tests:
        raise ValueError("negative conformance events contain duplicate test names")
    seen_negative_tests.add(event["test_name"])
    outcome = event.get("status")
    if outcome not in ("pass", "fail"):
        raise ValueError(f"negative conformance event[{index}].status is invalid")
    derived_outcome = "pass" if event["actual_decision"].lower() == event["expected_decision"] else "fail"
    if outcome != derived_outcome:
        raise ValueError(
            f"negative conformance event[{index}].status contradicts its decisions"
        )
    negative_derived[outcome] += 1
    uint(event.get("duration_ms"), f"negative conformance event[{index}].duration_ms")
    event_time = validate_input_timestamp(
        event.get("ts"), f"negative conformance event[{index}].ts", millis=True
    )
    if abs((event_time - negative_generated).total_seconds()) > 300:
        raise ValueError(f"negative conformance event[{index}] is not bound to its triage run")
negative_counts = negative_triage.get("counts")
if not isinstance(negative_counts, dict) or set(negative_counts) != set(negative_derived):
    raise ValueError("negative conformance triage counts have an invalid shape")
for field, expected in negative_derived.items():
    if uint(negative_counts.get(field), f"negative conformance triage counts.{field}") != expected:
        raise ValueError(f"negative conformance triage counts.{field} does not match retained events")
validate_rate(
    negative_triage.get("pass_rate_pct"),
    negative_derived["pass"],
    negative_derived["fail"],
    "negative conformance triage pass_rate_pct",
)


def source_blob_exists(relative):
    return relative in source_tracked_paths


def expected_report_log(suite, extension_id):
    candidates = (
        (
            f"tests/ext_conformance/reports/extensions/{extension_id}.jsonl",
            f"tests/ext_conformance/reports/smoke/extensions/{extension_id}.jsonl",
        )
        if suite == "smoke"
        else (f"tests/ext_conformance/reports/{suite}/extensions/{extension_id}.jsonl",)
    )
    for relative in candidates:
        if decision_source_present(relative, f"{suite} evidence log for {extension_id}"):
            raw = capture_decision_input(relative, f"{suite} evidence log for {extension_id}")
            parse_jsonl_document(raw, f"{suite} evidence log for {extension_id}")
            return relative
    return None


parity_log_paths = {}
parity_records_seen = set()
parity_error_records = []
parity_log_required_fields = {
    "scenario_id",
    "extension_id",
    "kind",
    "summary",
    "status",
    "source_tier",
    "runtime_tier",
    "ts_ms",
    "rust_ms",
}
parity_log_optional_fields = {
    "diffs",
    "ts_result",
    "rust_result",
    "error",
    "skip_reason",
}
for extension_id in expected_extension_ids:
    parity_log_relative = expected_report_log("parity", extension_id)
    main_for_extension = {
        key: value for key, value in parity_main_records.items() if key[0] == extension_id
    }
    if parity_log_relative is None:
        if main_for_extension:
            raise ValueError(f"parity events for {extension_id} lack a retained per-extension log")
        parity_log_paths[extension_id] = None
        continue
    parity_log_paths[extension_id] = parity_log_relative
    records = parse_jsonl_document(
        capture_decision_input(
            parity_log_relative, f"parity evidence log for {extension_id}"
        ),
        f"parity evidence log for {extension_id}",
    )
    derived_match = 0
    derived_mismatch = 0
    extension_record_keys = set()
    for record_index, record in enumerate(records):
        fields = set(record)
        if not parity_log_required_fields <= fields or not fields <= (
            parity_log_required_fields | parity_log_optional_fields
        ):
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}] has invalid fields"
            )
        if record.get("extension_id") != extension_id:
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}] has the wrong extension"
            )
        scenario_id = record.get("scenario_id")
        if not isinstance(scenario_id, str) or not scenario_id:
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}].scenario_id is invalid"
            )
        record_key = (extension_id, scenario_id)
        if record_key in parity_records_seen:
            raise ValueError(f"parity evidence logs duplicate {extension_id}/{scenario_id}")
        parity_records_seen.add(record_key)
        extension_record_keys.add(record_key)
        for field in ("kind", "summary", "runtime_tier"):
            if not isinstance(record.get(field), str) or not record[field]:
                raise ValueError(
                    f"parity evidence log for {extension_id} record[{record_index}].{field} is invalid"
                )
        if record.get("source_tier") != expected_extensions[extension_id]["source_tier"]:
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}].source_tier "
                "differs from the source manifest"
            )
        uint(
            record.get("ts_ms"),
            f"parity evidence log for {extension_id} record[{record_index}].ts_ms",
        )
        uint(
            record.get("rust_ms"),
            f"parity evidence log for {extension_id} record[{record_index}].rust_ms",
        )
        outcome = record.get("status")
        if outcome not in ("match", "mismatch", "ts_error", "rust_error", "skip"):
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}].status is invalid"
            )
        diffs = record.get("diffs", [])
        if not isinstance(diffs, list) or any(not isinstance(item, str) for item in diffs):
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}].diffs is invalid"
            )
        error = record.get("error")
        skip_reason = record.get("skip_reason")
        if error is not None and (not isinstance(error, str) or not error):
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}].error is invalid"
            )
        if skip_reason is not None and (not isinstance(skip_reason, str) or not skip_reason):
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}].skip_reason is invalid"
            )
        if outcome == "match":
            if diffs:
                raise ValueError(
                    f"parity evidence log for {extension_id} record[{record_index}] "
                    "reports a match with retained diffs"
                )
            derived_match += 1
        elif outcome == "mismatch":
            if not diffs:
                raise ValueError(
                    f"parity evidence log for {extension_id} record[{record_index}] "
                    "reports a mismatch without retained diffs"
                )
            derived_mismatch += 1
        elif outcome in ("ts_error", "rust_error"):
            if error is None:
                raise ValueError(
                    f"parity evidence log for {extension_id} record[{record_index}] "
                    "reports an error without diagnostics"
                )
            parity_error_records.append(f"{extension_id}/{scenario_id}:{outcome}")
            derived_mismatch += 1
        elif skip_reason is None:
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}] "
                "reports a skip without a reason"
            )

        if outcome in ("match", "mismatch") and (error is not None or skip_reason is not None):
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}] "
                "has diagnostics for the wrong outcome"
            )
        if outcome in ("ts_error", "rust_error") and (diffs or skip_reason is not None):
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}] "
                "has diagnostics for the wrong error outcome"
            )
        if outcome == "skip" and (diffs or error is not None):
            raise ValueError(
                f"parity evidence log for {extension_id} record[{record_index}] "
                "has diagnostics for the wrong skip outcome"
            )

        main_event = parity_main_records.get(record_key)
        if main_event is None:
            raise ValueError(
                f"parity evidence log outcome {extension_id}/{scenario_id} is missing "
                "from parity_events.jsonl"
            )
        for field in (
            "kind",
            "summary",
            "source_tier",
            "runtime_tier",
            "status",
            "ts_ms",
            "rust_ms",
        ):
            if main_event.get(field) != record.get(field):
                raise ValueError(
                    f"parity event {extension_id}/{scenario_id}.{field} disagrees with "
                    "the per-extension log"
                )
        for field, default in (("diffs", []), ("error", None), ("skip_reason", None)):
            if main_event.get(field, default) != record.get(field, default):
                raise ValueError(
                    f"parity event {extension_id}/{scenario_id}.{field} disagrees with "
                    "the per-extension log"
                )
    if extension_record_keys != set(main_for_extension):
        raise ValueError(
            f"parity_events.jsonl inventory for {extension_id} does not exactly match "
            "its per-extension log"
        )
    statuses[extension_id]["parity_match"] = derived_match
    statuses[extension_id]["parity_mismatch"] = derived_mismatch
if parity_error_records:
    raise ValueError(
        "parity evidence contains runtime/oracle errors: "
        + ", ".join(parity_error_records[:5])
    )


event_fields = {
    "schema",
    "ts",
    "extension_id",
    "version",
    "source_tier",
    "conformance_tier",
    "artifact_path",
    "evidence",
    "capabilities",
    "registrations",
    "overall_status",
    "rust_load_ms",
    "ts_load_ms",
    "load_ratio",
    "scenario_pass",
    "scenario_fail",
    "scenario_skip",
    "smoke_pass",
    "smoke_fail",
    "parity_match",
    "parity_mismatch",
    "failures",
}
events = parse_jsonl_document(raw_events, "conformance events")
if len(events) != len(expected_extension_ids):
    raise ValueError("conformance event inventory does not exactly cover the source manifest")
for index, event in enumerate(events):
    if set(event) != event_fields or event.get("schema") != "pi.ext.conformance_report.v2":
        raise ValueError(f"conformance event line {index + 1} has an invalid schema")
    extension_id = expected_extension_ids[index]
    expected = expected_extensions[extension_id]
    if event.get("extension_id") != extension_id:
        raise ValueError("conformance event identities/order do not match the source manifest")
    if event.get("source_tier") != expected["source_tier"]:
        raise ValueError(f"conformance event source tier differs for {extension_id}")
    if event.get("conformance_tier") != expected["conformance_tier"]:
        raise ValueError(f"conformance event conformance tier differs for {extension_id}")
    expected_artifact = f"tests/ext_conformance/artifacts/{expected['entry_path']}"
    if event.get("artifact_path") != expected_artifact or not source_blob_exists(expected_artifact):
        raise ValueError(f"conformance event artifact path differs from source manifest for {extension_id}")
    if event.get("capabilities") != expected["capabilities"]:
        raise ValueError(f"conformance event capabilities differ from source manifest for {extension_id}")
    if event.get("registrations") != expected["registrations"]:
        raise ValueError(f"conformance event registrations differ from source manifest for {extension_id}")
    expected_version = provenance_versions.get(extension_id)
    if event.get("version") != expected_version:
        raise ValueError(f"conformance event version differs from source provenance for {extension_id}")
    if event.get("overall_status") not in ("PASS", "FAIL", "N/A"):
        raise ValueError(f"conformance event status is invalid for {extension_id}")
    parsed_event_time = canonical_timestamp(event.get("ts"), f"conformance event ts for {extension_id}", millis=True)
    if abs((parsed_event_time - generated_at).total_seconds()) > 300:
        raise ValueError(f"conformance event timestamp is not bound to the summary run for {extension_id}")
    counters = {}
    for field in (
        "scenario_pass",
        "scenario_fail",
        "scenario_skip",
        "smoke_pass",
        "smoke_fail",
        "parity_match",
        "parity_mismatch",
    ):
        counters[field] = uint(event.get(field), f"conformance event {extension_id}.{field}")
    rust_load_ms = uint(
        event.get("rust_load_ms"), f"conformance event {extension_id}.rust_load_ms", nullable=True
    )
    ts_load_ms = uint(
        event.get("ts_load_ms"), f"conformance event {extension_id}.ts_load_ms", nullable=True
    )
    load_ratio = finite_number(
        event.get("load_ratio"), f"conformance event {extension_id}.load_ratio", nullable=True
    )
    if load_ratio is not None and load_ratio < 0:
        raise ValueError(f"conformance event {extension_id}.load_ratio must be non-negative")
    failures = event.get("failures")
    if not isinstance(failures, list) or any(not isinstance(item, str) for item in failures):
        raise ValueError(f"conformance event {extension_id}.failures must be an array of strings")
    has_scenario_results = counters["scenario_pass"] > 0 or counters["scenario_fail"] > 0
    effective_smoke_fail = 0 if has_scenario_results else counters["smoke_fail"]
    if counters["scenario_fail"] > 0 or effective_smoke_fail > 0 or counters["parity_mismatch"] > 0:
        derived_overall = "FAIL"
    elif counters["scenario_pass"] > 0 or counters["smoke_pass"] > 0 or counters["parity_match"] > 0:
        derived_overall = "PASS"
    elif rust_load_ms is not None:
        derived_overall = "PASS"
    else:
        derived_overall = "N/A"
    if event.get("overall_status") != derived_overall:
        raise ValueError(
            f"conformance event {extension_id} overall_status contradicts its retained counters"
        )
    expected_status = statuses[extension_id]
    for field, value in (
        *counters.items(),
        ("rust_load_ms", rust_load_ms),
        ("ts_load_ms", ts_load_ms),
        ("load_ratio", load_ratio),
        ("failures", failures),
    ):
        if value != expected_status[field]:
            raise ValueError(
                f"conformance event {extension_id}.{field} does not match retained raw decision sources"
            )
    evidence = event.get("evidence")
    if not isinstance(evidence, dict) or set(evidence) != {"fixture", "smoke_log", "parity_log"}:
        raise ValueError(f"conformance event {extension_id}.evidence has an invalid shape")
    fixture_relative = f"tests/ext_conformance/fixtures/{extension_id}.json"
    expected_fixture = fixture_relative if source_blob_exists(fixture_relative) else None
    expected_evidence = {
        "fixture": expected_fixture,
        "smoke_log": expected_report_log("smoke", extension_id),
        "parity_log": parity_log_paths[extension_id],
    }
    if evidence != expected_evidence:
        raise ValueError(f"conformance event {extension_id}.evidence differs from retained artifacts")

derived_status_counts = {
    "pass": sum(event["overall_status"] == "PASS" for event in events),
    "fail": sum(event["overall_status"] == "FAIL" for event in events),
    "na": sum(event["overall_status"] == "N/A" for event in events),
}
derived_tiers = {}
for source_tier, event in zip(expected_source_tiers, events, strict=True):
    tier = derived_tiers.setdefault(source_tier, {"total": 0, "pass": 0, "fail": 0, "na": 0})
    tier["total"] += 1
    tier[{"PASS": "pass", "FAIL": "fail", "N/A": "na"}[event["overall_status"]]] += 1
reported_tiers = data.get("per_tier")
if not isinstance(reported_tiers, dict) or set(reported_tiers) != set(derived_tiers):
    raise ValueError("conformance per_tier summary has an invalid tier inventory")
for tier_name, expected_tier in derived_tiers.items():
    reported_tier = reported_tiers.get(tier_name)
    if not isinstance(reported_tier, dict) or set(reported_tier) != {"total", "pass", "fail", "na"}:
        raise ValueError(f"conformance per_tier.{tier_name} has an invalid shape")
    for field, expected in expected_tier.items():
        if uint(reported_tier.get(field), f"conformance per_tier.{tier_name}.{field}") != expected:
            raise ValueError(
                f"conformance per_tier.{tier_name}.{field} does not match source-bound events"
            )
if reported_tiers != dict(sorted(derived_tiers.items())):
    raise ValueError("conformance per_tier summary does not match the source-bound event inventory")

counts = data.get('counts', {})
if not isinstance(counts, dict) or set(counts) != {"total", "pass", "fail", "na", "tested"}:
    raise ValueError("counts must contain exactly total/pass/fail/na/tested")

def count(name):
    value = counts.get(name)
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= 2**63 - 1:
        raise ValueError(f"counts.{name} must be a non-negative signed 64-bit integer")
    return value

total = count("total")
passed = count("pass")
failed = count("fail")
not_applicable = count("na")
tested = passed + failed
if total != tested + not_applicable:
    raise ValueError("counts.total must equal counts.pass + counts.fail + counts.na")
if counts["tested"] != tested:
    raise ValueError("counts.tested must equal counts.pass + counts.fail")
if total != len(expected_extension_ids):
    raise ValueError("counts.total does not match the source conformance manifest")
if (
    passed != derived_status_counts["pass"]
    or failed != derived_status_counts["fail"]
    or not_applicable != derived_status_counts["na"]
):
    raise ValueError("conformance counts do not match the source-bound event inventory")

pass_rate = data.get("pass_rate_pct")
if isinstance(pass_rate, bool) or not isinstance(pass_rate, (int, float)) or not math.isfinite(pass_rate):
    raise ValueError("pass_rate_pct must be a finite number")
if not 0 <= pass_rate <= 100:
    raise ValueError("pass_rate_pct must be in the range 0..100")
expected_rate = 100.0 * passed / tested if tested else 0.0
if not math.isclose(float(pass_rate), expected_rate, rel_tol=1e-9, abs_tol=1e-9):
    raise ValueError(
        f"pass_rate_pct={pass_rate} is inconsistent with pass/fail counts (expected {expected_rate})"
    )

coverage_rate = data.get("coverage_rate_pct")
if isinstance(coverage_rate, bool) or not isinstance(coverage_rate, (int, float)) or not math.isfinite(coverage_rate):
    raise ValueError("coverage_rate_pct must be a finite number")
expected_coverage = 100.0 * tested / total
if not math.isclose(float(coverage_rate), expected_coverage, rel_tol=1e-9, abs_tol=1e-9):
    raise ValueError(
        f"coverage_rate_pct={coverage_rate} is inconsistent with tested/total counts (expected {expected_coverage})"
    )

negative_summary = data.get("negative")
if not isinstance(negative_summary, dict) or set(negative_summary) != {"pass", "fail"}:
    raise ValueError("negative fields do not match the canonical summary contract")
for field in ("pass", "fail"):
    if uint(negative_summary.get(field), f"negative.{field}") != negative_derived[field]:
        raise ValueError(f"negative.{field} does not match retained negative conformance evidence")
if negative_summary["fail"] != 0:
    raise ValueError("negative.fail must be zero for release admission")

derived_evidence = {
    "golden_fixtures": sum(event["evidence"]["fixture"] is not None for event in events),
    "smoke_logs": sum(event["evidence"]["smoke_log"] is not None for event in events),
    "parity_logs": sum(event["evidence"]["parity_log"] is not None for event in events),
    "load_time_benchmarks": sum(event["rust_load_ms"] is not None for event in events),
}
reported_evidence = data.get("evidence")
if not isinstance(reported_evidence, dict) or set(reported_evidence) != set(derived_evidence):
    raise ValueError("evidence fields do not match the canonical summary contract")
for field, expected in derived_evidence.items():
    if uint(reported_evidence.get(field), f"evidence.{field}") != expected:
        raise ValueError(f"evidence.{field} does not match validated event evidence")

rate_display = format(float(pass_rate), ".12g")
rate_passes = int(float(pass_rate) >= minimum_rate)

verify_repository_binding()
final_head = git("rev-parse", "--verify", "HEAD^{commit}").decode("ascii", "strict").strip()
if final_head != head:
    raise ValueError("release HEAD changed during conformance validation")
final_head_entry = [
    record
    for record in git("ls-tree", "-z", "HEAD", "--", summary_relative).split(b"\0")
    if record
]
if final_head_entry != head_entry:
    raise ValueError("conformance summary tree entry changed during validation")
final_index_entry = [
    record
    for record in git("ls-files", "--stage", "-z", "--", summary_relative).split(b"\0")
    if record
]
if final_index_entry != index_entry:
    raise ValueError("conformance summary index entry changed during validation")
final_flags = [
    record
    for record in git("ls-files", "-v", "-z", "--", summary_relative).split(b"\0")
    if record
]
if final_flags != flags:
    raise ValueError("conformance summary index flags changed during validation")
try:
    final_metadata = summary_path.lstat()
    final_summary = summary_path.read_bytes()
except OSError as exc:
    raise ValueError(f"unable to re-read conformance summary: {exc}") from exc
if not stat.S_ISREG(final_metadata.st_mode):
    raise ValueError("conformance summary became a symlink or special file during validation")
if os.name != "nt" and final_metadata.st_mode & 0o111:
    raise ValueError("conformance summary became executable during validation")
if final_summary != raw_summary:
    raise ValueError("conformance summary bytes changed during validation")
final_framed = b"blob " + str(len(final_summary)).encode("ascii") + b"\0" + final_summary
final_oid = (
    hashlib.sha1(final_framed).hexdigest().encode("ascii")
    if len(head_oid) == 40
    else hashlib.sha256(final_framed).hexdigest().encode("ascii")
)
if final_oid != head_oid:
    raise ValueError("conformance summary bytes no longer match release HEAD")
for relative, initial in decision_inputs.items():
    label, initial_path, initial_raw, initial_tree, initial_index, initial_flags = initial
    final_path, final_raw, final_tree, final_index, final_input_flags = capture_head_bound_file(
        relative, label
    )
    if final_path != initial_path or final_raw != initial_raw:
        raise ValueError(f"{label} bytes changed during validation")
    if final_tree != initial_tree:
        raise ValueError(f"{label} tree entry changed during validation")
    if final_index != initial_index:
        raise ValueError(f"{label} index entry changed during validation")
    if final_input_flags != initial_flags:
        raise ValueError(f"{label} index flags changed during validation")
for relative in absent_decision_paths:
    if os.path.lexists(root / relative):
        raise ValueError(f"absent conformance decision source appeared during validation: {relative}")
    if relative in head_tracked_paths:
        raise ValueError(f"absent conformance decision source became tracked during validation: {relative}")

print(
    total,
    passed,
    failed,
    not_applicable,
    tested,
    rate_display,
    rate_passes,
    source_commit,
    run_id,
    correlation_id,
    sep="\t",
)
PY
    ); then
        IFS=$'\t' read -r TOTAL _PASS FAIL NA TESTED PASS_RATE PASS_RATE_OK CONFORMANCE_SOURCE CONFORMANCE_RUN_ID CONFORMANCE_CORRELATION_ID <<< "$SUMMARY_DATA"

        check_pass "conformance_provenance" "schema=v2 source=$CONFORMANCE_SOURCE run=$CONFORMANCE_RUN_ID correlation=$CONFORMANCE_CORRELATION_ID age<=${MAX_EVIDENCE_AGE_HOURS}h"

        if [[ "$TOTAL" -eq 0 ]]; then
            check_fail "conformance_total" "Zero total scenarios in conformance summary"
        else
            check_pass "conformance_total" "$TOTAL total scenarios"
        fi

        if [[ "$TESTED" -eq 0 ]]; then
            check_fail "conformance_pass_rate" "No pass/fail scenarios were executed"
        elif [[ "$PASS_RATE_OK" -eq 1 ]]; then
            check_pass "conformance_pass_rate" "${PASS_RATE}% >= ${MIN_PASS_RATE}% threshold"
        else
            check_fail "conformance_pass_rate" "${PASS_RATE}% < ${MIN_PASS_RATE}% threshold"
        fi

        if [[ "$FAIL" -le "$MAX_FAIL_COUNT" ]]; then
            check_pass "conformance_fail_count" "$FAIL failures <= $MAX_FAIL_COUNT threshold"
        else
            check_fail "conformance_fail_count" "$FAIL failures > $MAX_FAIL_COUNT threshold"
        fi

        if [[ "$NA" -le "$MAX_NA_COUNT" ]]; then
            check_pass "conformance_na_count" "$NA N/A <= $MAX_NA_COUNT threshold"
        else
            check_fail "conformance_na_count" "$NA N/A > $MAX_NA_COUNT threshold"
        fi
    else
        check_fail "conformance_summary" "Invalid conformance_summary.json: $SUMMARY_DATA"
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

# Gate 6: Performance-claim readiness. The evidence contract itself is always
# fail-closed. A coherent BLOCKED result is a warning only when this release is
# explicitly configured to make no quantitative or global performance claim.
PERFORMANCE_SUMMARY="$PROJECT_ROOT/tests/perf/reports/budget_summary.json"
if [[ -f "$PERFORMANCE_SUMMARY" ]]; then
    if PERFORMANCE_CHECK=$(python3 - "$PROJECT_ROOT" "$PERFORMANCE_SUMMARY" "$REQUIRE_PERFORMANCE_CLAIM_READY" "$MAX_EVIDENCE_AGE_HOURS" 2>&1 <<'PY'
import fnmatch
import hashlib
import json
import math
import os
import re
import stat
import subprocess
import sys
import tomllib
from datetime import datetime, timedelta, timezone
from pathlib import Path

raw_project_root = Path(sys.argv[1])
supplied_summary_path = Path(sys.argv[2])
claim_ready_required = sys.argv[3] == "1"
maximum_age = timedelta(hours=int(sys.argv[4]))
PERFORMANCE_SUMMARY_RELATIVE_PATH = "tests/perf/reports/budget_summary.json"

TOP_LEVEL_FIELDS = {
    "schema",
    "generated_at",
    "source_commit",
    "run_id",
    "correlation_id",
    "strict_mode",
    "total_budgets",
    "ci_enforced",
    "ci_with_data",
    "ci_fail",
    "ci_no_data",
    "pass",
    "fail",
    "no_data",
    "data_contract_failures_count",
    "failing_data_contracts",
    "budgets",
    "budget_results",
    "claim_readiness",
}
BUDGET_FIELDS = {
    "name",
    "category",
    "metric",
    "unit",
    "threshold",
    "comparison",
    "methodology",
    "ci_enforced",
}
RESULT_REQUIRED_FIELDS = {
    "budget_name",
    "category",
    "threshold",
    "comparison",
    "unit",
    "actual",
    "status",
    "source",
    "ci_enforced",
}
RESULT_OPTIONAL_FIELDS = {"failure_reason"}
FAILURE_REQUIRED_FIELDS = {"contract_id", "detail", "remediation"}
FAILURE_OPTIONAL_FIELDS = {"budget_name"}
CLAIM_READINESS_FIELDS = {
    "status",
    "performance_claims_authorized",
    "blocking_reason_codes",
}
LINEAGE_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:/-]{0,255}")
OBJECT_ID_RE = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})")
TIMESTAMP_RE = re.compile(
    r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z"
)
CANONICAL_BUDGET_INVENTORY_SHA256 = "96e3147ef23e1c634d56265581975a2b619ac9a701f4839ef6f3f4b3987226ad"


class ContractError(ValueError):
    pass


def finish(status, detail):
    print(f"{status}|{str(detail).replace(chr(10), ' ')}")
    raise SystemExit(0)


def fail(detail):
    raise ContractError(detail)


def resolve_repository_context():
    if raw_project_root.is_symlink() or not raw_project_root.is_dir():
        fail("performance repository root must be a real directory, not a symlink")
    try:
        resolved_root = raw_project_root.resolve(strict=True)
        git_marker = resolved_root / ".git"
        marker_metadata = git_marker.lstat()
        if stat.S_ISLNK(marker_metadata.st_mode):
            fail("performance repository .git marker must not be a symlink")
        if stat.S_ISDIR(marker_metadata.st_mode):
            resolved_git_dir = git_marker.resolve(strict=True)
        elif stat.S_ISREG(marker_metadata.st_mode):
            marker = git_marker.read_text(encoding="utf-8").rstrip("\r\n")
            if "\n" in marker or "\r" in marker or not marker.startswith("gitdir: "):
                fail("performance repository .git file is malformed")
            target = Path(marker.removeprefix("gitdir: "))
            candidate = target if target.is_absolute() else resolved_root / target
            target_metadata = candidate.lstat()
            if stat.S_ISLNK(target_metadata.st_mode) or not stat.S_ISDIR(target_metadata.st_mode):
                fail("performance repository gitfile target must be a non-symlink directory")
            resolved_git_dir = candidate.resolve(strict=True)
        else:
            fail("performance repository .git marker is not a directory or gitfile")
    except (OSError, RuntimeError, UnicodeError) as exc:
        fail(f"performance repository Git context could not be resolved safely: {exc}")
    if not resolved_git_dir.is_dir():
        fail("performance repository Git directory is not a directory")
    return resolved_root, resolved_git_dir


def reject_duplicate_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            fail(f"duplicate JSON object key: {key}")
        value[key] = item
    return value


def exact_fields(value, expected, label, optional=frozenset()):
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    actual = set(value)
    missing = expected - actual
    extra = actual - expected - optional
    if missing or extra:
        fail(
            f"{label} fields are not exact "
            f"(missing={sorted(missing)}, unexpected={sorted(extra)})"
        )
    return value


def nonempty_string(value, label):
    if not isinstance(value, str) or not value.strip() or value != value.strip():
        fail(f"{label} must be a non-empty, surrounding-whitespace-free string")
    return value


def uint(value, label):
    if isinstance(value, bool) or not isinstance(value, int) or not 0 <= value <= 2**63 - 1:
        fail(f"{label} must be a non-negative signed 64-bit integer")
    return value


def finite_number(value, label, *, positive=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        fail(f"{label} must be a finite number")
    result = float(value)
    if not math.isfinite(result) or (positive and result <= 0.0):
        qualifier = "positive finite" if positive else "finite"
        fail(f"{label} must be a {qualifier} number")
    return result


def nullable_lineage(value, label):
    if value is None:
        return None
    if not isinstance(value, str) or LINEAGE_RE.fullmatch(value) is None:
        fail(f"{label} must be null or a canonical lineage identifier")
    return value


def canonical_budget_inventory_json(budgets):
    records = []
    for budget in budgets:
        threshold = float(budget["threshold"])
        if threshold != round(threshold, 6):
            fail(f"budget {budget['name']} threshold exceeds canonical six-decimal precision")
        records.append(
            "{" + ",".join(
                (
                    '"name":' + json.dumps(budget["name"], ensure_ascii=False),
                    '"category":' + json.dumps(budget["category"], ensure_ascii=False),
                    '"metric":' + json.dumps(budget["metric"], ensure_ascii=False),
                    '"unit":' + json.dumps(budget["unit"], ensure_ascii=False),
                    f'"threshold":{threshold:.6f}',
                    '"comparison":' + json.dumps(budget["comparison"], ensure_ascii=False),
                    '"ci_enforced":' + ("true" if budget["ci_enforced"] else "false"),
                    '"methodology":' + json.dumps(budget["methodology"], ensure_ascii=False),
                )
            ) + "}"
        )
    return "[" + ",".join(records) + "]"


def run_git_result(*args):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_LITERAL_PATHSPECS"] = "1"
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    env["GIT_OPTIONAL_LOCKS"] = "0"
    env["GIT_TERMINAL_PROMPT"] = "0"
    return subprocess.run(
        [
            "git",
            "--git-dir", str(git_dir),
            "--work-tree", str(project_root),
            "-c", "core.bare=false",
            "-c", "core.fsmonitor=false",
            "-c", f"core.worktree={project_root}",
            *args,
        ],
        capture_output=True,
        env=env,
        check=False,
    )


def run_git(*args):
    result = run_git_result(*args)
    if result.returncode != 0:
        diagnostic = result.stderr.decode("utf-8", "replace").strip()
        fail(f"git {' '.join(args)} failed: {diagnostic}")
    return result.stdout


def verify_repository_binding():
    bindings = (
        (("rev-parse", "--show-toplevel"), project_root, "worktree"),
        (("rev-parse", "--absolute-git-dir"), git_dir, "Git directory"),
    )
    for args, expected, label in bindings:
        output = run_git(*args).decode("utf-8", "strict")
        if not output.endswith("\n") or "\n" in output.removesuffix("\n"):
            fail(f"performance repository {label} output is not one canonical line")
        try:
            reported = Path(output.removesuffix("\n")).resolve(strict=True)
        except (OSError, RuntimeError) as exc:
            fail(f"unable to canonicalize performance repository {label}: {exc}")
        if reported != expected:
            fail(
                f"performance repository {label} does not match the filesystem-derived binding"
            )


def canonical_head():
    head = run_git("rev-parse", "--verify", "HEAD^{commit}").decode(
        "ascii", "strict"
    ).strip()
    if OBJECT_ID_RE.fullmatch(head) is None:
        fail(f"release HEAD is not a canonical full lowercase Git object ID: {head!r}")
    return head


def validate_performance_artifact_at_head(expected_head=None):
    head = canonical_head()
    if expected_head is not None and head != expected_head:
        fail("release HEAD changed during performance summary validation")

    expected_path = project_root / PERFORMANCE_SUMMARY_RELATIVE_PATH
    if supplied_summary_path != expected_path:
        fail("performance summary path is not the canonical repository-relative artifact path")
    candidate = project_root
    for component in Path(PERFORMANCE_SUMMARY_RELATIVE_PATH).parts:
        candidate /= component
        try:
            metadata = candidate.lstat()
        except OSError as exc:
            fail(f"performance summary path component is unavailable: {candidate}: {exc}")
        if stat.S_ISLNK(metadata.st_mode):
            fail("performance summary path must not contain symlink components")
    if not stat.S_ISREG(candidate.lstat().st_mode):
        fail("performance summary must be a regular file")

    tree_bytes = run_git(
        "ls-tree",
        "--full-tree",
        "-z",
        head,
        "--",
        PERFORMANCE_SUMMARY_RELATIVE_PATH,
    )
    entries = [entry for entry in tree_bytes.split(b"\0") if entry]
    if len(entries) != 1:
        fail("performance summary is not tracked exactly once at release HEAD")
    try:
        metadata, tracked_path = entries[0].split(b"\t", 1)
    except ValueError:
        fail("performance summary release HEAD entry is malformed")
    fields = metadata.split(b" ")
    if (
        len(fields) != 3
        or fields[0] not in (b"100644", b"100755")
        or fields[1] != b"blob"
        or tracked_path != PERFORMANCE_SUMMARY_RELATIVE_PATH.encode("utf-8")
    ):
        fail("performance summary release HEAD entry must be the exact regular-file blob")

    live_metadata = candidate.lstat()
    live_mode = b"100755" if live_metadata.st_mode & 0o111 else b"100644"
    if live_mode != fields[0]:
        fail("performance summary worktree mode does not exactly match release HEAD")
    head_bytes = run_git("cat-file", "blob", fields[2].decode("ascii", "strict"))
    live_bytes = candidate.read_bytes()
    if live_bytes != head_bytes:
        fail("performance summary raw worktree bytes do not exactly match release HEAD")
    return head, head_bytes


def finish_validated(status, detail):
    final_head, final_bytes = validate_performance_artifact_at_head(artifact_head)
    if final_head != artifact_head or final_bytes != artifact_bytes:
        fail("performance summary binding changed during validation")
    finish(status, detail)


def package_includes(path, patterns):
    for raw_pattern in patterns:
        if not isinstance(raw_pattern, str) or not raw_pattern:
            fail("source Cargo.toml package.include entries must be non-empty strings")
        pattern = raw_pattern.removeprefix("/")
        if fnmatch.fnmatchcase(path, pattern):
            return True
        if pattern.endswith("/**") and path.startswith(pattern[:-3].rstrip("/") + "/"):
            return True
    return False


def verify_claim_source_binding(source_commit, head):
    if canonical_head() != head:
        fail("release HEAD changed during performance source binding validation")
    resolved = run_git("rev-parse", "--verify", f"{source_commit}^{{commit}}").decode(
        "ascii", "strict"
    ).strip()
    if resolved != source_commit:
        fail("source_commit does not resolve to the exact recorded commit")
    ancestor = run_git_result("merge-base", "--is-ancestor", source_commit, head)
    if ancestor.returncode == 1:
        fail("performance source commit is not an ancestor of release HEAD")
    if ancestor.returncode != 0:
        fail("unable to verify performance source ancestry")
    if source_commit == head:
        return
    try:
        cargo_document = tomllib.loads(
            run_git("show", f"{source_commit}:Cargo.toml").decode("utf-8", "strict")
        )
    except (UnicodeError, tomllib.TOMLDecodeError) as exc:
        fail(f"unable to parse source Cargo.toml package include policy: {exc}")
    package_patterns = cargo_document.get("package", {}).get("include", [])
    if not isinstance(package_patterns, list):
        fail("source Cargo.toml package.include must be an array")
    changed_paths = [
        os.fsdecode(path)
        for path in run_git("diff", "--name-only", "-z", "--no-renames", source_commit, head).split(b"\0")
        if path
    ]
    if not changed_paths:
        fail("source_commit differs from HEAD but the source-to-release diff is empty")
    for path in changed_paths:
        evidence_only = (
            path.startswith("tests/perf/reports/")
            or path.startswith("tests/e2e_results/")
            or path.startswith("tests/ext_conformance/reports/")
            or path.startswith("tests/certification/")
            or path.startswith("docs/evidence/")
        )
        if not evidence_only:
            fail(f"non-evidence path changed after source_commit: {path}")
        if path.startswith("docs/evidence/") and package_includes(path, package_patterns):
            fail(f"packaged or product-consumed evidence changed after source_commit: {path}")


try:
    project_root, git_dir = resolve_repository_context()
    verify_repository_binding()
    artifact_head, artifact_bytes = validate_performance_artifact_at_head()
    raw = artifact_bytes.decode("utf-8", "strict")
    data = json.loads(raw, object_pairs_hook=reject_duplicate_keys)
    exact_fields(data, TOP_LEVEL_FIELDS, "performance summary")
    if data["schema"] != "pi.perf.budget_summary.v2":
        fail(f"unsupported performance summary schema: {data['schema']!r}")

    generated_at_raw = data["generated_at"]
    if not isinstance(generated_at_raw, str) or TIMESTAMP_RE.fullmatch(generated_at_raw) is None:
        fail("generated_at must be canonical UTC RFC3339 ending in Z")
    generated_at = datetime.fromisoformat(generated_at_raw.removesuffix("Z") + "+00:00")
    if generated_at.utcoffset() != timedelta(0):
        fail("generated_at must use UTC")
    if generated_at.isoformat(timespec="milliseconds").replace("+00:00", "Z") != generated_at_raw:
        fail("generated_at must use canonical millisecond-precision UTC RFC3339")
    now = datetime.now(timezone.utc)
    if generated_at > now + timedelta(minutes=5):
        fail("performance summary timestamp is more than five minutes in the future")

    source_commit = data["source_commit"]
    if source_commit is not None and (
        not isinstance(source_commit, str) or OBJECT_ID_RE.fullmatch(source_commit) is None
    ):
        fail("source_commit must be null or a canonical full lowercase Git object ID")
    if source_commit is not None:
        verify_claim_source_binding(source_commit, artifact_head)
    run_id = nullable_lineage(data["run_id"], "run_id")
    correlation_id = nullable_lineage(data["correlation_id"], "correlation_id")
    if run_id != correlation_id:
        fail("run_id and correlation_id must both be null or match")
    strict_mode = data["strict_mode"]
    if not isinstance(strict_mode, bool):
        fail("strict_mode must be a boolean")

    count_names = (
        "total_budgets",
        "ci_enforced",
        "ci_with_data",
        "ci_fail",
        "ci_no_data",
        "pass",
        "fail",
        "no_data",
        "data_contract_failures_count",
    )
    counts = {name: uint(data[name], name) for name in count_names}

    budgets = data["budgets"]
    results = data["budget_results"]
    failures = data["failing_data_contracts"]
    if not isinstance(budgets, list) or not budgets:
        fail("budgets must be a non-empty array")
    if not isinstance(results, list) or not results:
        fail("budget_results must be a non-empty array")
    if not isinstance(failures, list):
        fail("failing_data_contracts must be an array")

    budgets_by_name = {}
    for index, budget in enumerate(budgets):
        exact_fields(budget, BUDGET_FIELDS, f"budgets[{index}]")
        name = nonempty_string(budget["name"], f"budgets[{index}].name")
        if name in budgets_by_name:
            fail(f"duplicate budget name: {name}")
        for field in ("category", "metric", "unit", "methodology"):
            nonempty_string(budget[field], f"budgets[{index}].{field}")
        finite_number(budget["threshold"], f"budgets[{index}].threshold", positive=True)
        if budget["comparison"] not in ("maximum", "minimum"):
            fail(f"budgets[{index}].comparison must be 'maximum' or 'minimum'")
        if not isinstance(budget["ci_enforced"], bool):
            fail(f"budgets[{index}].ci_enforced must be a boolean")
        budgets_by_name[name] = budget

    inventory_json = canonical_budget_inventory_json(budgets)
    inventory_sha256 = hashlib.sha256(inventory_json.encode("utf-8")).hexdigest()
    if inventory_sha256 != CANONICAL_BUDGET_INVENTORY_SHA256:
        fail(
            "budget inventory does not match the canonical producer contract "
            f"(observed_sha256={inventory_sha256}, "
            f"expected_sha256={CANONICAL_BUDGET_INVENTORY_SHA256})"
        )

    results_by_name = {}
    status_counts = {"PASS": 0, "FAIL": 0, "NO_DATA": 0}
    ci_with_data = 0
    ci_fail = 0
    ci_no_data = 0
    for index, result in enumerate(results):
        exact_fields(
            result,
            RESULT_REQUIRED_FIELDS,
            f"budget_results[{index}]",
            RESULT_OPTIONAL_FIELDS,
        )
        name = nonempty_string(result["budget_name"], f"budget_results[{index}].budget_name")
        if name in results_by_name:
            fail(f"duplicate budget result: {name}")
        budget = budgets_by_name.get(name)
        if budget is None:
            fail(f"budget result has no matching definition: {name}")
        for field in ("category", "unit", "comparison"):
            nonempty_string(result[field], f"budget_results[{index}].{field}")
            if result[field] != budget[field]:
                fail(f"budget result {name} has mismatched {field}")
        threshold = finite_number(
            result["threshold"], f"budget_results[{index}].threshold", positive=True
        )
        if threshold != float(budget["threshold"]):
            fail(f"budget result {name} has mismatched threshold")
        if not isinstance(result["ci_enforced"], bool):
            fail(f"budget_results[{index}].ci_enforced must be a boolean")
        if result["ci_enforced"] is not budget["ci_enforced"]:
            fail(f"budget result {name} has mismatched ci_enforced")
        nonempty_string(result["source"], f"budget_results[{index}].source")
        status = result["status"]
        if status not in status_counts:
            fail(f"budget result {name} has unsupported status: {status!r}")
        failure_reason_present = "failure_reason" in result
        failure_reason = result.get("failure_reason")
        if failure_reason_present:
            nonempty_string(failure_reason, f"budget_results[{index}].failure_reason")
        actual = result["actual"]
        if actual is None:
            if strict_mode and budget["ci_enforced"]:
                if status != "FAIL" or failure_reason != "missing_measurement_data":
                    fail(
                        f"strict CI budget {name} without data must be FAIL with "
                        "failure_reason=missing_measurement_data"
                    )
            elif status != "NO_DATA" or failure_reason_present:
                fail(f"budget {name} without data must be NO_DATA without a failure reason")
        else:
            actual_value = finite_number(actual, f"budget_results[{index}].actual")
            if actual_value < 0.0:
                fail(f"budget_results[{index}].actual must be non-negative")
            passes = (
                actual_value >= threshold
                if budget["comparison"] == "minimum"
                else actual_value <= threshold
            )
            expected_status = "PASS" if passes else "FAIL"
            if status != expected_status:
                fail(
                    f"budget result {name} status={status} is inconsistent with "
                    f"actual={actual_value} and threshold={threshold}"
                )
            if failure_reason_present:
                fail(f"budget result {name} with data must not contain failure_reason")
        status_counts[status] += 1
        if budget["ci_enforced"]:
            ci_with_data += int(actual is not None)
            ci_fail += int(status == "FAIL")
            ci_no_data += int(status == "NO_DATA")
        results_by_name[name] = result

    if list(results_by_name) != list(budgets_by_name):
        missing = sorted(set(budgets_by_name) - set(results_by_name))
        unexpected = sorted(set(results_by_name) - set(budgets_by_name))
        fail(
            "budget_results must match canonical budget declaration order and membership "
            f"(missing={missing}, unexpected={unexpected})"
        )

    failure_fingerprints = set()
    for index, failure in enumerate(failures):
        exact_fields(
            failure,
            FAILURE_REQUIRED_FIELDS,
            f"failing_data_contracts[{index}]",
            FAILURE_OPTIONAL_FIELDS,
        )
        values = []
        for field in ("contract_id", "detail", "remediation"):
            values.append(nonempty_string(failure[field], f"failing_data_contracts[{index}].{field}"))
        budget_name = failure.get("budget_name")
        if budget_name is not None:
            budget_name = nonempty_string(
                budget_name, f"failing_data_contracts[{index}].budget_name"
            )
            if budget_name not in budgets_by_name:
                fail(f"data-contract failure references unknown budget: {budget_name}")
        fingerprint = (*values, budget_name)
        if fingerprint in failure_fingerprints:
            fail(f"duplicate data-contract failure at index {index}")
        failure_fingerprints.add(fingerprint)

    derived_counts = {
        "total_budgets": len(budgets),
        "ci_enforced": sum(int(budget["ci_enforced"]) for budget in budgets),
        "ci_with_data": ci_with_data,
        "ci_fail": ci_fail,
        "ci_no_data": ci_no_data,
        "pass": status_counts["PASS"],
        "fail": status_counts["FAIL"],
        "no_data": status_counts["NO_DATA"],
        "data_contract_failures_count": len(failures),
    }
    for name, expected in derived_counts.items():
        if counts[name] != expected:
            fail(f"{name}={counts[name]} is inconsistent with derived value {expected}")
    if counts["pass"] + counts["fail"] + counts["no_data"] != counts["total_budgets"]:
        fail("pass + fail + no_data must equal total_budgets")

    claim = exact_fields(data["claim_readiness"], CLAIM_READINESS_FIELDS, "claim_readiness")
    reasons = claim["blocking_reason_codes"]
    if not isinstance(reasons, list) or any(not isinstance(reason, str) or not reason for reason in reasons):
        fail("claim_readiness.blocking_reason_codes must be an array of non-empty strings")
    if reasons != sorted(set(reasons)):
        fail("claim_readiness.blocking_reason_codes must be sorted and duplicate-free")

    expected_reasons = []
    if counts["no_data"] != 0:
        expected_reasons.append("budget_data_missing")
    if counts["fail"] != 0:
        expected_reasons.append("budget_failed")
    if counts["ci_with_data"] != counts["ci_enforced"] or counts["ci_no_data"] != 0:
        expected_reasons.append("ci_budget_data_missing")
    if counts["ci_fail"] != 0:
        expected_reasons.append("ci_budget_failed")
    if correlation_id is None:
        expected_reasons.append("correlation_id_missing")
    if counts["data_contract_failures_count"] != 0:
        expected_reasons.append("data_contract_failure")
    if run_id is None:
        expected_reasons.append("run_id_missing")
    if source_commit is None:
        expected_reasons.append("source_commit_unbound")
    if not strict_mode:
        expected_reasons.append("strict_mode_disabled")
    expected_reasons.sort()
    if reasons != expected_reasons:
        fail(
            "claim_readiness.blocking_reason_codes do not match derived blockers "
            f"(reported={reasons}, expected={expected_reasons})"
        )

    claim_ready = not expected_reasons
    expected_status = "claim_ready" if claim_ready else "blocked"
    if claim["status"] != expected_status:
        fail(f"claim_readiness.status must be {expected_status!r}")
    if claim["performance_claims_authorized"] is not claim_ready:
        fail(f"claim_readiness.performance_claims_authorized must be {claim_ready}")

    if now - generated_at > maximum_age:
        fail(f"performance summary is stale ({now - generated_at} old; maximum {maximum_age})")

    if claim_ready:
        finish_validated(
            "pass",
            f"performance claims authorized: all {counts['total_budgets']} declared budgets "
            f"have data and pass; strict v2 evidence source={source_commit} run={run_id} "
            f"correlation={correlation_id} age<={sys.argv[4]}h",
        )

    detail = (
        "performance claims are NOT authorized; valid blocked/NO_DATA evidence "
        f"(blocking_reason_codes={','.join(expected_reasons)})"
    )
    if claim_ready_required:
        finish_validated("fail", f"{detail}; RELEASE_GATE_REQUIRE_PERFORMANCE_CLAIM_READY=1")
    finish_validated(
        "warn", f"{detail}; release must make no quantitative or global performance claims"
    )
except (ContractError, OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
    finish("fail", f"invalid performance budget summary: {exc}")
PY
    ); then
        :
    else
        PERFORMANCE_CHECK="fail|unexpected performance summary validator error: $PERFORMANCE_CHECK"
    fi
else
    PERFORMANCE_CHECK="fail|tests/perf/reports/budget_summary.json not found"
fi

PERFORMANCE_STATUS="${PERFORMANCE_CHECK%%|*}"
PERFORMANCE_DETAIL="${PERFORMANCE_CHECK#*|}"
case "$PERFORMANCE_STATUS" in
    pass)
        check_pass "performance_claim_readiness" "$PERFORMANCE_DETAIL"
        CANONICAL_PERF_TEST="checked_in_budget_summary_matches_fresh_canonical_evaluation_exactly"
        CANONICAL_PERF_LIST_OUTPUT=""
        CANONICAL_PERF_RUN_OUTPUT=""
        CANONICAL_PERF_LIST_VALID=0
        CANONICAL_PERF_RUN_VALID=0
        if CANONICAL_PERF_LIST_OUTPUT=$(
            CARGO_TERM_COLOR=never run_cargo_gate test --locked --test perf_budgets \
                "$CANONICAL_PERF_TEST" -- --exact --list --format terse 2>&1
        ); then
            if printf '%s\n' "$CANONICAL_PERF_LIST_OUTPUT" \
                | validate_exact_libtest_output list "$CANONICAL_PERF_TEST"; then
                CANONICAL_PERF_LIST_VALID=1
            fi
        fi
        if [[ "$CANONICAL_PERF_LIST_VALID" -eq 1 ]] && CANONICAL_PERF_RUN_OUTPUT=$(
            PI_PERF_STRICT=1 CARGO_TERM_COLOR=never \
                run_cargo_gate test --locked --test perf_budgets \
                "$CANONICAL_PERF_TEST" -- --exact --nocapture --test-threads=1 2>&1
        ); then
            if printf '%s\n' "$CANONICAL_PERF_RUN_OUTPUT" \
                | validate_exact_libtest_output run "$CANONICAL_PERF_TEST"; then
                CANONICAL_PERF_RUN_VALID=1
            fi
        fi
        if [[ "$CANONICAL_PERF_LIST_VALID" -eq 1 && "$CANONICAL_PERF_RUN_VALID" -eq 1 ]]; then
            check_pass "performance_claim_canonical_contract" "Canonical strict perf data readers independently confirm every declared budget and linked data contract"
        else
            check_fail "performance_claim_canonical_contract" "Canonical strict perf contract did not list and execute exactly one non-ignored test successfully; summary cannot authorize performance claims"
        fi
        ;;
    warn)
        check_warn "performance_claim_readiness" "$PERFORMANCE_DETAIL"
        ;;
    fail)
        check_fail "performance_claim_readiness" "$PERFORMANCE_DETAIL"
        ;;
    *)
        check_fail "performance_claim_readiness" "unexpected performance summary validation result: $PERFORMANCE_CHECK"
        ;;
esac

# Gate 7: Compilation check (cargo check)
if run_cargo_gate check --locked --lib --quiet 2>/dev/null; then
    check_pass "cargo_check" "Library compiles cleanly"
else
    check_fail "cargo_check" "cargo check --lib failed"
fi

# Gate 8: Clippy lint
if run_cargo_gate clippy --locked --lib --quiet -- -D warnings 2>/dev/null; then
    check_pass "clippy" "No clippy warnings"
else
    check_fail "clippy" "Clippy has warnings"
fi

# Gate 9: Preflight analyzer (optional)
if [[ "$REQUIRE_PREFLIGHT" -eq 1 ]]; then
    if run_cargo_gate test --locked --lib extension_preflight --quiet 2>/dev/null; then
        check_pass "preflight_tests" "Extension preflight tests pass"
    else
        check_fail "preflight_tests" "Extension preflight tests failed"
    fi
fi

# Gate 10: Quality pipeline (optional)
if [[ "$REQUIRE_QUALITY" -eq 1 ]]; then
    quality_runner_flag=()
    if [[ "$CARGO_RUNNER_MODE" == "rch" ]]; then
        quality_runner_flag=(--require-rch)
    elif [[ "$CARGO_RUNNER_REQUEST" == "local" ]]; then
        quality_runner_flag=(--no-rch)
    fi
    if "$SCRIPT_DIR/ext_quality_pipeline.sh" --check-only --report "${quality_runner_flag[@]}" >/dev/null 2>&1; then
        check_pass "quality_pipeline" "Extension quality pipeline passes"
    else
        check_fail "quality_pipeline" "Extension quality pipeline failed"
    fi
fi

# Gate 11: Suite classification guard
CLASSIFICATION="$PROJECT_ROOT/tests/suite_classification.toml"
if [[ -f "$CLASSIFICATION" ]]; then
    check_pass "suite_classification" "suite_classification.toml exists"
else
    check_fail "suite_classification" "suite_classification.toml missing"
fi

# Gate 12: Traceability matrix
TRACEABILITY="$PROJECT_ROOT/docs/traceability_matrix.json"
if [[ -f "$TRACEABILITY" ]]; then
    check_pass "traceability_matrix" "traceability_matrix.json exists"
else
    check_warn "traceability_matrix" "traceability_matrix.json not found"
fi

# Gate 13: Drop-in certification contract artifact
DROPIN_CONTRACT="$PROJECT_ROOT/docs/contracts/dropin-certification-contract.json"
if [[ -f "$DROPIN_CONTRACT" ]]; then
    if CONTRACT_CHECK=$(python3 - "$DROPIN_CONTRACT" 2>&1 <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])

def reject_duplicate_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object key: {key}")
        value[key] = item
    return value

try:
    data = json.loads(
        path.read_text(encoding="utf-8"),
        object_pairs_hook=reject_duplicate_keys,
    )
except Exception as exc:  # noqa: BLE001
    print(f"parse_error:{exc}")
    raise SystemExit(0)

if not isinstance(data, dict):
    print("invalid:root must be an object")
    raise SystemExit(0)

missing = []
for key in ("schema", "hard_gates", "release_process_enforcement"):
    if key not in data:
        missing.append(key)

enforcement = data.get("release_process_enforcement")
if not isinstance(enforcement, dict):
    print("invalid:release_process_enforcement must be an object")
    raise SystemExit(0)
contract = enforcement.get("verdict_artifact_contract")
if not isinstance(contract, dict):
    print("invalid:verdict_artifact_contract must be an object")
    raise SystemExit(0)
for key in ("path", "schema", "required_fields", "blocking_rule"):
    if key not in contract:
        missing.append(f"release_process_enforcement.verdict_artifact_contract.{key}")

if missing:
    print("missing:" + ",".join(missing))
    raise SystemExit(0)

if data.get("schema") != "pi.dropin.certification_contract.v1":
    print(f"schema_mismatch:{data.get('schema')}")
    raise SystemExit(0)

print("ok")
PY
    ); then
        :
    else
        CONTRACT_CHECK="invalid:unexpected validator error: $CONTRACT_CHECK"
    fi

    case "$CONTRACT_CHECK" in
        ok)
            check_pass "dropin_contract" "dropin certification contract is present and well-formed"
            ;;
        parse_error:*)
            check_fail "dropin_contract" "dropin certification contract JSON parse failed (${CONTRACT_CHECK#parse_error:})"
            ;;
        missing:*)
            check_fail "dropin_contract" "dropin certification contract missing required fields (${CONTRACT_CHECK#missing:})"
            ;;
        schema_mismatch:*)
            check_fail "dropin_contract" "unexpected contract schema (${CONTRACT_CHECK#schema_mismatch:})"
            ;;
        invalid:*)
            check_fail "dropin_contract" "dropin certification contract is invalid (${CONTRACT_CHECK#invalid:})"
            ;;
        *)
            check_fail "dropin_contract" "unexpected contract validation result: $CONTRACT_CHECK"
            ;;
    esac
else
    check_fail "dropin_contract" "docs/contracts/dropin-certification-contract.json not found"
fi

# Gate 14: Drop-in certification verdict (required for strict claim mode)
DROPIN_VERDICT="$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json"
if DROPIN_CHECK=$(python3 - "$PROJECT_ROOT" "$DROPIN_CONTRACT" "$DROPIN_VERDICT" "$REQUIRE_DROPIN_CERTIFIED" "$MAX_EVIDENCE_AGE_HOURS" 2>&1 <<'PY'
import fnmatch
import json
import os
import re
import stat
import subprocess
import sys
import tomllib
from datetime import datetime, timedelta, timezone
from pathlib import Path, PurePosixPath

raw_project_root = Path(sys.argv[1])
contract_path = Path(sys.argv[2])
verdict_path = Path(sys.argv[3])
# IMPORTANT: this must track the shell-resolved gate toggle derived from
# RELEASE_GATE_REQUIRE_DROPIN_CERTIFIED. Reading an unrelated env var here
# can silently disable strict drop-in enforcement.
strict_required = sys.argv[4] == "1"
max_evidence_age = timedelta(hours=int(sys.argv[5]))
certification_claimed = False
DROPIN_CONTRACT_RELATIVE = "docs/contracts/dropin-certification-contract.json"
DROPIN_VERDICT_RELATIVE = "docs/evidence/dropin-certification-verdict.json"
DROPIN_LANE_RELATIVE = "tests/full_suite_gate/certification_verdict.json"
decision_inputs = []
current_head = None

def finish(status, detail):
    print(f"{status}|{detail}")
    raise SystemExit(0)

def reject_duplicate_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object key: {key}")
        value[key] = item
    return value

def resolve_repository_context():
    if raw_project_root.is_symlink() or not raw_project_root.is_dir():
        finish("fail", "drop-in repository root must be a real directory, not a symlink")
    try:
        resolved_root = raw_project_root.resolve(strict=True)
        git_marker = resolved_root / ".git"
        marker_metadata = git_marker.lstat()
        if stat.S_ISLNK(marker_metadata.st_mode):
            finish("fail", "drop-in repository .git marker must not be a symlink")
        if stat.S_ISDIR(marker_metadata.st_mode):
            resolved_git_dir = git_marker.resolve(strict=True)
        elif stat.S_ISREG(marker_metadata.st_mode):
            marker = git_marker.read_text(encoding="utf-8").rstrip("\r\n")
            if "\n" in marker or "\r" in marker or not marker.startswith("gitdir: "):
                finish("fail", "drop-in repository .git file is malformed")
            target = Path(marker.removeprefix("gitdir: "))
            candidate = target if target.is_absolute() else resolved_root / target
            target_metadata = candidate.lstat()
            if stat.S_ISLNK(target_metadata.st_mode) or not stat.S_ISDIR(target_metadata.st_mode):
                finish("fail", "drop-in repository gitfile target must be a non-symlink directory")
            resolved_git_dir = candidate.resolve(strict=True)
        else:
            finish("fail", "drop-in repository .git marker is not a directory or gitfile")
    except (OSError, RuntimeError, UnicodeError) as exc:
        finish("fail", f"drop-in repository Git context could not be resolved safely: {exc}")
    if not resolved_git_dir.is_dir():
        finish("fail", "drop-in repository Git directory is not a directory")
    return resolved_root, resolved_git_dir

def run_git(*args, text=True):
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_LITERAL_PATHSPECS"] = "1"
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    env["GIT_OPTIONAL_LOCKS"] = "0"
    env["GIT_TERMINAL_PROMPT"] = "0"
    return subprocess.run(
        [
            "git",
            "--git-dir", str(git_dir),
            "--work-tree", str(project_root),
            "-c", "core.bare=false",
            "-c", "core.fsmonitor=false",
            "-c", f"core.worktree={project_root}",
            *args,
        ],
        capture_output=True,
        text=text,
        env=env,
        check=False,
    )

project_root, git_dir = resolve_repository_context()

def verify_repository_binding():
    bindings = (
        (("rev-parse", "--show-toplevel"), project_root, "worktree"),
        (("rev-parse", "--absolute-git-dir"), git_dir, "Git directory"),
    )
    for args, expected, label in bindings:
        result = run_git(*args)
        if result.returncode != 0:
            finish("fail", f"unable to verify canonical drop-in repository {label}")
        output = result.stdout
        if not output.endswith("\n") or "\n" in output.removesuffix("\n"):
            finish("fail", f"drop-in repository {label} output is not one canonical line")
        try:
            reported = Path(output.removesuffix("\n")).resolve(strict=True)
        except (OSError, RuntimeError) as exc:
            finish("fail", f"unable to canonicalize drop-in repository {label}: {exc}")
        if reported != expected:
            finish("fail", f"drop-in repository {label} does not match the filesystem-derived binding")

verify_repository_binding()

def canonical_input_metadata(path, relative, label):
    expected_path = project_root / relative
    if path != expected_path:
        finish("fail", f"{label} path is not the canonical repository path: {relative}")
    current = project_root
    try:
        root_metadata = current.lstat()
        if stat.S_ISLNK(root_metadata.st_mode) or not stat.S_ISDIR(root_metadata.st_mode):
            finish("fail", "drop-in repository root must be a real directory, not a symlink")
        parts = Path(relative).parts
        for index, component in enumerate(parts):
            current /= component
            metadata = current.lstat()
            if stat.S_ISLNK(metadata.st_mode):
                finish("fail", f"{label} path must not contain symlink components: {relative}")
            if index + 1 < len(parts) and not stat.S_ISDIR(metadata.st_mode):
                finish("fail", f"{label} parent component is not a directory: {current}")
    except FileNotFoundError:
        finish("fail", f"{label} is missing: {relative}")
    except OSError as exc:
        finish("fail", f"unable to inspect {label}: {exc}")
    if not stat.S_ISREG(metadata.st_mode):
        finish("fail", f"{label} must be a regular non-symlink file: {relative}")
    if os.name != "nt" and metadata.st_mode & 0o111:
        finish("fail", f"{label} must not be executable: {relative}")
    return metadata

def load_json_input(path, relative, label):
    canonical_input_metadata(path, relative, label)
    try:
        raw_bytes = path.read_bytes()
        payload = json.loads(
            raw_bytes.decode("utf-8"),
            object_pairs_hook=reject_duplicate_keys,
        )
    except Exception as exc:  # noqa: BLE001
        finish("fail", f"{label} parse error: {exc}")
    if not isinstance(payload, dict):
        finish("fail", f"{label} root must be an object")
    return payload, raw_bytes

def canonical_current_head():
    head_check = run_git("rev-parse", "--verify", "HEAD^{commit}")
    if head_check.returncode != 0:
        finish("fail", "unable to resolve the current release HEAD")
    head = head_check.stdout.strip()
    if re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", head) is None:
        finish("fail", f"current release HEAD is not a canonical full object ID: {head!r}")
    return head

def verify_decision_inputs(expected_head):
    if canonical_current_head() != expected_head:
        finish("fail", "release HEAD changed while drop-in decision inputs were being validated")
    for path, relative, label, captured_bytes in decision_inputs:
        canonical_input_metadata(path, relative, label)

        tree_entry = run_git("ls-tree", "-z", expected_head, "--", relative, text=False)
        if tree_entry.returncode != 0:
            finish("fail", f"unable to inspect committed {label}: {relative}")
        records = [record for record in tree_entry.stdout.split(b"\0") if record]
        if len(records) != 1:
            finish("fail", f"{label} must have exactly one entry in release HEAD: {relative}")
        try:
            metadata, recorded_path = records[0].split(b"\t", 1)
            mode, object_type, object_id = metadata.split(b" ", 2)
        except ValueError:
            finish("fail", f"{label} has a malformed release HEAD entry: {relative}")
        if mode != b"100644" or object_type != b"blob" or os.fsdecode(recorded_path) != relative:
            finish("fail", f"{label} must be a canonical non-executable JSON blob in release HEAD: {relative}")

        index_entry = run_git("ls-files", "--stage", "-z", "--", relative, text=False)
        if index_entry.returncode != 0:
            finish("fail", f"unable to inspect {label} index entry: {relative}")
        index_records = [record for record in index_entry.stdout.split(b"\0") if record]
        if len(index_records) != 1:
            finish("fail", f"{label} must have exactly one canonical index entry: {relative}")
        try:
            index_metadata, index_path = index_records[0].split(b"\t", 1)
            index_mode, index_object_id, index_stage = index_metadata.split(b" ", 2)
        except ValueError:
            finish("fail", f"{label} has a malformed index entry: {relative}")
        if (
            index_mode != mode
            or index_object_id != object_id
            or index_stage != b"0"
            or os.fsdecode(index_path) != relative
        ):
            finish("fail", f"{label} index entry differs from release HEAD: {relative}")

        flags = run_git("ls-files", "-v", "-z", "--", relative, text=False)
        if flags.returncode != 0:
            finish("fail", f"unable to inspect {label} index flags: {relative}")
        flag_records = [record for record in flags.stdout.split(b"\0") if record]
        if flag_records != [b"H " + os.fsencode(relative)]:
            finish("fail", f"{label} has non-canonical index flags: {relative}")

        committed = run_git("cat-file", "blob", os.fsdecode(object_id), text=False)
        if committed.returncode != 0:
            finish("fail", f"unable to read committed {label}: {relative}")
        try:
            current_bytes = path.read_bytes()
        except OSError as exc:
            finish("fail", f"unable to read current {label}: {exc}")
        if committed.stdout != captured_bytes:
            finish("fail", f"{label} bytes parsed by the validator differ from release HEAD: {relative}")
        if current_bytes != captured_bytes:
            finish("fail", f"{label} changed while it was being validated: {relative}")

        diff = run_git("diff", "--quiet", "HEAD", "--", relative)
        if diff.returncode == 1:
            finish("fail", f"{label} index/worktree differs from release HEAD: {relative}")
        if diff.returncode != 0:
            finish("fail", f"unable to inspect current {label} state: {relative}")

    if canonical_current_head() != expected_head:
        finish("fail", "release HEAD changed during the final drop-in decision-input recheck")

def finish_verified(status, detail):
    expected_inputs = 3 if certification_claimed else 2
    if current_head is None or len(decision_inputs) != expected_inputs:
        finish("fail", "drop-in decision-input verification was not initialized")
    verify_decision_inputs(current_head)
    finish(status, detail)

def provenance_failure(detail):
    if strict_required or certification_claimed:
        finish("fail", detail)
    finish_verified("warn", f"{detail} (strict drop-in mode disabled)")

def package_includes(path, patterns):
    for raw_pattern in patterns:
        if not isinstance(raw_pattern, str) or not raw_pattern:
            finish("fail", "source Cargo.toml package.include entries must be non-empty strings")
        pattern = raw_pattern.removeprefix("/")
        if fnmatch.fnmatchcase(path, pattern):
            return True
        if pattern.endswith("/**") and path.startswith(pattern[:-3].rstrip("/") + "/"):
            return True
    return False

def canonical_repo_path(relative):
    if (
        not isinstance(relative, str)
        or not relative
        or "\\" in relative
        or re.match(r"^[A-Za-z]:", relative) is not None
    ):
        return None
    pure = PurePosixPath(relative)
    if pure.is_absolute() or pure.as_posix() != relative or any(part in ("", ".", "..") for part in pure.parts):
        return None
    return pure

contract, contract_bytes = load_json_input(
    contract_path,
    DROPIN_CONTRACT_RELATIVE,
    "drop-in contract",
)
decision_inputs.append(
    (contract_path, DROPIN_CONTRACT_RELATIVE, "drop-in contract", contract_bytes)
)

enforcement = contract.get("release_process_enforcement")
if not isinstance(enforcement, dict):
    finish("fail", "contract release_process_enforcement must be an object")
spec = enforcement.get("verdict_artifact_contract")
if not isinstance(spec, dict):
    finish("fail", "contract verdict_artifact_contract must be an object")
required_fields = spec.get("required_fields", [])
expected_schema = spec.get("schema", "pi.dropin.certification_verdict.v1")
expected_verdict_path = spec.get("path")
if (
    not isinstance(required_fields, list)
    or not required_fields
    or any(not isinstance(field, str) or not field for field in required_fields)
    or len(required_fields) != len(set(required_fields))
):
    finish("fail", "contract verdict required_fields must be a non-empty array of unique strings")
if not isinstance(expected_schema, str) or not expected_schema:
    finish("fail", "contract verdict schema must be a non-empty string")
if expected_schema != "pi.dropin.certification_verdict.v1":
    finish("fail", f"contract names an unsupported verdict schema: {expected_schema}")
if expected_verdict_path != "docs/evidence/dropin-certification-verdict.json":
    finish("fail", "contract verdict path does not name docs/evidence/dropin-certification-verdict.json")
required_verdict_fields = {
    "git_commit",
    "generated_at_utc",
    "overall_verdict",
    "hard_gate_results",
    "blocking_reasons",
    "evidence_index",
}
if set(required_fields) != required_verdict_fields:
    finish("fail", "contract verdict required_fields do not match the supported v1 schema")

try:
    verdict_path.lstat()
except FileNotFoundError:
    if strict_required:
        finish("fail", f"{DROPIN_VERDICT_RELATIVE} is missing in strict drop-in mode")
    current_head = canonical_current_head()
    verify_decision_inputs(current_head)
    try:
        verdict_path.lstat()
    except FileNotFoundError:
        pass
    except OSError as exc:
        finish("fail", f"unable to re-inspect missing drop-in verdict: {exc}")
    else:
        finish("fail", "drop-in verdict appeared while its absence was being validated")
    verify_decision_inputs(current_head)
    finish(
        "warn",
        f"{DROPIN_VERDICT_RELATIVE} is absent (strict drop-in mode disabled)",
    )
except OSError as exc:
    finish("fail", f"unable to inspect drop-in verdict: {exc}")

verdict, verdict_bytes = load_json_input(
    verdict_path,
    DROPIN_VERDICT_RELATIVE,
    "drop-in verdict",
)
decision_inputs.append(
    (verdict_path, DROPIN_VERDICT_RELATIVE, "drop-in verdict", verdict_bytes)
)

missing_fields = [field for field in required_fields if field not in verdict]
if missing_fields:
    finish("fail", "verdict missing required fields: " + ", ".join(missing_fields))

schema = verdict.get("schema")
if schema != expected_schema:
    finish("fail", f"verdict schema mismatch: expected {expected_schema}, got {schema}")

overall = verdict.get("overall_verdict")
if overall not in ("CERTIFIED", "NOT_CERTIFIED"):
    finish("fail", f"overall_verdict={overall!r} (expected CERTIFIED or NOT_CERTIFIED)")
certification_claimed = overall == "CERTIFIED"
if strict_required and overall != "CERTIFIED":
    finish("fail", f"overall_verdict={overall} (expected CERTIFIED in strict mode)")

generated_at = verdict.get("generated_at_utc")
if (
    not isinstance(generated_at, str)
    or re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?Z", generated_at)
    is None
):
    finish("fail", "generated_at_utc must be a canonical RFC3339 UTC timestamp ending in Z")
try:
    generated_at_time = datetime.fromisoformat(generated_at.removesuffix("Z") + "+00:00")
except ValueError:
    finish("fail", "generated_at_utc must be a valid RFC3339 UTC timestamp")
now = datetime.now(timezone.utc)
if generated_at_time > now + timedelta(minutes=5):
    finish("fail", "generated_at_utc is more than five minutes in the future")
if now - generated_at_time > max_evidence_age:
    finish(
        "fail",
        f"generated_at_utc is older than the configured {int(max_evidence_age.total_seconds() // 3600)}h evidence limit",
    )

verdict_commit = verdict.get("git_commit")
if not isinstance(verdict_commit, str) or re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", verdict_commit) is None:
    finish("fail", "git_commit must be a full lowercase Git object ID")

current_head = canonical_current_head()
verify_decision_inputs(current_head)

commit_check = run_git("rev-parse", "--verify", f"{verdict_commit}^{{commit}}")
if commit_check.returncode != 0:
    finish("fail", f"git_commit={verdict_commit} is not a commit in this repository")
if commit_check.stdout.strip() != verdict_commit:
    finish("fail", f"git_commit={verdict_commit} did not resolve exactly")

if verdict_commit != current_head:
    ancestor_check = run_git("merge-base", "--is-ancestor", verdict_commit, current_head)
    if ancestor_check.returncode == 1:
        provenance_failure(
            f"historical drop-in verdict git_commit={verdict_commit} is not an ancestor of current release HEAD={current_head}"
        )
    if ancestor_check.returncode != 0:
        finish("fail", "unable to inspect drop-in verdict commit ancestry")

    allowed_prefixes = (
        b"docs/evidence/",
        b"tests/ext_conformance/reports/",
        b"tests/perf/reports/",
        b"tests/cross_platform_reports/",
        b"tests/franken_node_compat/reports/",
        b"tests/evidence_bundle/",
        b"tests/certification/",
    )
    cargo_source = run_git("show", f"{verdict_commit}:Cargo.toml")
    if cargo_source.returncode != 0:
        finish("fail", "unable to load verdict-source Cargo.toml package include policy")
    try:
        package_patterns = tomllib.loads(cargo_source.stdout).get("package", {}).get("include", [])
    except tomllib.TOMLDecodeError as exc:
        finish("fail", f"unable to parse verdict-source Cargo.toml package include policy: {exc}")
    if not isinstance(package_patterns, list):
        finish("fail", "verdict-source Cargo.toml package.include must be an array")
    history = run_git(
        "diff",
        "--name-only",
        "-z",
        "--no-renames",
        verdict_commit,
        current_head,
        text=False,
    )
    if history.returncode != 0:
        finish("fail", "unable to inspect commits following the drop-in verdict source commit")
    changed_paths = [path for path in history.stdout.split(b"\0") if path]
    disallowed = []
    for path in changed_paths:
        decoded = os.fsdecode(path)
        if not path.startswith(allowed_prefixes):
            disallowed.append(path)
        elif path.startswith(b"docs/evidence/") and package_includes(decoded, package_patterns):
            disallowed.append(path)
    if disallowed:
        examples = ", ".join(os.fsdecode(path) for path in disallowed[:5])
        provenance_failure(
            f"historical drop-in verdict for git_commit={verdict_commit}; current release HEAD={current_head} "
            f"contains non-evidence follow-up changes: {examples}"
        )

hard_gate_results = verdict.get("hard_gate_results")
if strict_required or certification_claimed:
    if not isinstance(hard_gate_results, list) or not hard_gate_results:
        finish("fail", "hard_gate_results missing/empty in strict mode")

    gate_id_pattern = re.compile(r"G(0[1-9]|1[0-2])-[a-z0-9]+(?:-[a-z0-9]+)*")
    expected_gate_specs = []
    contract_hard_gates = contract.get("hard_gates")
    if not isinstance(contract_hard_gates, list):
        finish("fail", "contract hard_gates must be an array")
    if len(contract_hard_gates) != 12:
        finish("fail", f"contract must define exactly the ordered G01-G12 gate set; found {len(contract_hard_gates)}")
    for index, gate in enumerate(contract_hard_gates, start=1):
        if not isinstance(gate, dict):
            finish("fail", f"contract hard_gates[{index - 1}] must be an object")
        gate_id = gate.get("gate_id")
        match = gate_id_pattern.fullmatch(gate_id) if isinstance(gate_id, str) else None
        if match is None or int(match.group(1)) != index:
            finish("fail", f"contract hard_gates[{index - 1}] must be canonical gate G{index:02d}")
        blocking = gate.get("blocking")
        if not isinstance(blocking, bool):
            finish("fail", f"contract hard gate {gate_id} blocking must be boolean")
        bead = gate.get("owner_issue_primary")
        if not isinstance(bead, str) or not bead:
            finish("fail", f"contract hard gate {gate_id} owner_issue_primary must be non-empty")
        required_artifacts = gate.get("required_artifacts")
        if not isinstance(required_artifacts, list) or not required_artifacts:
            finish("fail", f"contract hard gate {gate_id} has no required_artifacts")
        canonical_artifacts = []
        for artifact in required_artifacts:
            canonical_artifact = canonical_repo_path(artifact)
            if canonical_artifact is None:
                finish("fail", f"contract hard gate {gate_id} has an invalid required_artifact: {artifact!r}")
            canonical_artifacts.append(canonical_artifact.as_posix())
        if len(canonical_artifacts) != len(set(canonical_artifacts)):
            finish("fail", f"contract hard gate {gate_id} repeats a required_artifact")
        expected_gate_specs.append(
            {
                "gate_id": gate_id,
                "blocking": blocking,
                "bead": bead,
                "required_artifacts": canonical_artifacts,
            }
        )

    if len(hard_gate_results) != len(expected_gate_specs):
        finish(
            "fail",
            f"hard_gate_results must contain exactly {len(expected_gate_specs)} ordered G01-G12 entries",
        )
    non_pass = []
    for index, (gate, expected) in enumerate(zip(hard_gate_results, expected_gate_specs, strict=True)):
        if not isinstance(gate, dict) or not isinstance(gate.get("gate_id"), str) or not gate["gate_id"]:
            finish("fail", f"hard_gate_results[{index}] must be an object with a non-empty gate_id")
        gate_id = gate["gate_id"]
        if gate_id != expected["gate_id"]:
            finish(
                "fail",
                f"hard_gate_results[{index}] identity mismatch: expected {expected['gate_id']}, got {gate_id}",
            )
        status_value = gate.get("status")
        status = status_value if isinstance(status_value, str) else ""
        if status not in ("pass", "fail", "blocked", "waived"):
            finish("fail", f"hard gate {gate_id} has invalid status: {status_value!r}")
        if gate.get("blocking") is not expected["blocking"]:
            finish("fail", f"hard gate {gate_id} blocking flag differs from the contract")
        detail = gate.get("detail")
        if detail is not None and not isinstance(detail, str):
            finish("fail", f"hard gate {gate_id} detail must be a string or null")
        bead = gate.get("bead")
        if bead != expected["bead"]:
            finish("fail", f"hard gate {gate_id} bead differs from the contract owner")
        artifact_paths = gate.get("artifact_paths")
        if artifact_paths != expected["required_artifacts"]:
            finish("fail", f"hard gate {gate_id} artifact_paths differ from the contract")
        if status != "pass":
            non_pass.append(f"{gate_id}:{status or 'unknown'}")
    if non_pass:
        finish("fail", "non-pass hard gates in strict mode: " + ", ".join(non_pass))

    blocking_reasons = verdict.get("blocking_reasons")
    if not isinstance(blocking_reasons, list):
        finish("fail", "blocking_reasons must be an array in strict mode")
    if blocking_reasons:
        finish("fail", "blocking_reasons is non-empty in strict mode")

    source = verdict.get("source")
    if not isinstance(source, dict):
        finish("fail", "source must be an object in strict mode")
    if source.get("certification_lane_artifact") != "tests/full_suite_gate/certification_verdict.json":
        finish("fail", "source.certification_lane_artifact is not the canonical certification lane artifact")
    if source.get("lane_schema") != "pi.ci.certification_lane.v1":
        finish("fail", f"source.lane_schema={source.get('lane_schema')!r} (expected 'pi.ci.certification_lane.v1')")
    if source.get("lane_verdict") != "pass":
        finish("fail", f"source.lane_verdict={source.get('lane_verdict')!r} (expected 'pass')")

    lane_path = project_root / DROPIN_LANE_RELATIVE
    lane, lane_bytes = load_json_input(
        lane_path,
        DROPIN_LANE_RELATIVE,
        "drop-in certification lane",
    )
    decision_inputs.append(
        (lane_path, DROPIN_LANE_RELATIVE, "drop-in certification lane", lane_bytes)
    )
    if lane.get("schema") != "pi.ci.certification_lane.v1":
        finish(
            "fail",
            f"actual certification lane schema={lane.get('schema')!r} "
            "(expected 'pi.ci.certification_lane.v1')",
        )
    expected_lane_fields = {
        "schema",
        "lane",
        "generated_at",
        "verdict",
        "policy",
        "gates",
        "waiver_audit",
        "waivers_applied",
        "summary",
        "promotion_rules",
        "rerun_guidance",
    }
    if set(lane) != expected_lane_fields:
        finish("fail", "actual certification lane top-level fields do not match the canonical contract")
    if lane.get("lane") != "full":
        finish("fail", f"actual certification lane lane={lane.get('lane')!r} (expected 'full')")
    expected_policy = (
        "Full certification: all blocking gates must pass for release. "
        "Waived gates are tracked but do not block. Expired waivers fail the waiver_lifecycle gate."
    )
    if lane.get("policy") != expected_policy:
        finish("fail", "actual certification lane policy does not match the canonical full-lane policy")
    lane_generated_raw = lane.get("generated_at")
    if (
        not isinstance(lane_generated_raw, str)
        or re.fullmatch(
            r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z",
            lane_generated_raw,
        )
        is None
    ):
        finish("fail", "actual certification lane generated_at must use canonical millisecond UTC precision")
    try:
        lane_generated = datetime.fromisoformat(lane_generated_raw.removesuffix("Z") + "+00:00")
    except ValueError:
        finish("fail", "actual certification lane generated_at is invalid")
    if lane_generated > now + timedelta(minutes=5):
        finish("fail", "actual certification lane generated_at is more than five minutes in the future")
    if now - lane_generated > max_evidence_age:
        finish("fail", "actual certification lane evidence is stale")
    if abs((generated_at_time - lane_generated).total_seconds()) > 300:
        finish("fail", "drop-in verdict and actual certification lane timestamps differ by more than five minutes")

    expected_lane_gates = [
        ("non_mock_unit", "Non-mock unit compliance", "bd-1f42.2.6", True, "docs/non-mock-rubric.json", "cargo test --test non_mock_compliance_gate -- --nocapture"),
        ("e2e_log_contract", "E2E log contract and transcripts", "bd-1f42.3.6", False, "tests/e2e_results", None),
        ("ext_must_pass", "Extension must-pass gate", "bd-1f42.4.4", True, "tests/ext_conformance/reports/gate/must_pass_gate_verdict.json", "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_must_pass_gate --nocapture --exact"),
        ("ext_provider_compat", "Extension provider compatibility matrix", "bd-1f42.4.6", False, "tests/ext_conformance/reports/provider_compat/provider_compat_report.json", "cargo test --test ext_conformance_generated --features ext-conformance -- conformance_provider_compat_matrix --nocapture --exact"),
        ("evidence_bundle", "Unified evidence bundle", "bd-1f42.6.8", False, "tests/evidence_bundle/index.json", "cargo test --test ci_evidence_bundle -- build_evidence_bundle --nocapture --exact"),
        ("cross_platform", "Cross-platform matrix validation", "bd-1f42.6.7", True, "tests/cross_platform_reports/linux/platform_report.json", "cargo test --test ci_cross_platform_matrix -- cross_platform_matrix --nocapture --exact"),
        ("conformance_regression", "Conformance regression gate", "bd-1f42.4", True, "tests/ext_conformance/reports/regression_verdict.json", "cargo test --test conformance_regression_gate -- --nocapture"),
        ("conformance_pass_rate", "Conformance pass rate >= 80%", "bd-1f42.4", True, "tests/ext_conformance/reports/conformance_summary.json", "cargo test --test conformance_report -- --nocapture"),
        ("suite_classification", "Suite classification guard", "bd-1f42.6.1", True, "tests/suite_classification.toml", None),
        ("traceability_matrix", "Requirement traceability matrix", "bd-1f42.6.4", False, "docs/traceability_matrix.json", None),
        ("e2e_scenario_matrix", "Canonical E2E scenario matrix", "bd-1f42.8.5.1", False, "docs/e2e_scenario_matrix.json", "python3 scripts/check_traceability_matrix.py"),
        ("provider_gap_matrix", "Provider gap test matrix coverage", "bd-3uqg.11.11.5", False, "docs/provider-gaps-test-matrix.json", "cargo test --test provider_native_contract --test e2e_provider_scenarios -- --nocapture"),
        ("sec_conformance", "SEC-6.4 security compatibility conformance", "bd-1a2cu", True, "tests/full_suite_gate/sec_conformance_verdict.json", "cargo test --test sec_compatibility_conformance -- --nocapture"),
        ("perf3x_bead_coverage", "PERF-3X bead-to-artifact coverage audit", "bd-3ar8v.6.11", True, "tests/full_suite_gate/perf3x_bead_coverage_audit.json", "cargo test --test ci_full_suite_gate -- perf3x_bead_coverage_contract_is_well_formed --nocapture --exact"),
        ("practical_finish_checkpoint", "Practical-finish checkpoint (docs-only residual filter)", "bd-3ar8v.6.9", True, "tests/full_suite_gate/practical_finish_checkpoint.json", "cargo test --test ci_full_suite_gate -- practical_finish_report_fails_when_technical_open_issues_remain --nocapture --exact"),
        ("extension_remediation_backlog", "Extension remediation backlog artifact integrity", "bd-3ar8v.6.8", True, "tests/full_suite_gate/extension_remediation_backlog.json", "cargo test --test qa_certification_dossier -- certification_dossier --nocapture --exact"),
        ("opportunity_matrix_integrity", "Opportunity matrix artifact integrity", "bd-3ar8v.6.1", True, "tests/perf/reports/opportunity_matrix.json", "cargo test --test release_evidence_gate -- phase1_weighted_attribution_contract_links_phase5_consumers --nocapture --exact"),
        ("parameter_sweeps_integrity", "Parameter sweeps artifact integrity", "bd-3ar8v.6.2", True, "tests/perf/reports/parameter_sweeps.json", "cargo test --test release_evidence_gate -- parameter_sweeps_contract_links_phase1_matrix_and_readiness --nocapture --exact"),
        ("conformance_stress_lineage", "Conformance+stress lineage coherence", "bd-3ar8v.6.3", True, "tests/ext_conformance/reports/conformance_summary.json", "cargo test --test ci_full_suite_gate -- conformance_stress_lineage_passes_with_valid_artifacts --nocapture --exact"),
        ("waiver_lifecycle", "Waiver lifecycle compliance", "bd-1f42.8.8.1", True, "tests/full_suite_gate/waiver_audit.json", "cargo test --test ci_full_suite_gate -- waiver_lifecycle_audit --nocapture --exact"),
    ]
    lane_gates = lane.get("gates")
    if not isinstance(lane_gates, list) or len(lane_gates) != len(expected_lane_gates):
        finish("fail", f"actual certification lane must contain exactly {len(expected_lane_gates)} canonical gates")
    allowed_gate_fields = {"id", "name", "bead", "status", "blocking", "artifact_path", "detail", "reproduce_command"}
    required_gate_fields = {"id", "name", "bead", "status", "blocking", "artifact_path"}
    for index, (gate, expected) in enumerate(zip(lane_gates, expected_lane_gates, strict=True)):
        if not isinstance(gate, dict) or not required_gate_fields.issubset(gate) or not set(gate).issubset(allowed_gate_fields):
            finish("fail", f"actual certification lane gate[{index}] has invalid fields")
        gate_id, name, bead, blocking, artifact_path, reproduce_command = expected
        if (
            gate.get("id") != gate_id
            or gate.get("name") != name
            or gate.get("bead") != bead
            or gate.get("status") != "pass"
            or gate.get("blocking") is not blocking
            or gate.get("artifact_path") != artifact_path
            or gate.get("reproduce_command") != reproduce_command
        ):
            finish("fail", f"actual certification lane gate[{index}] does not match canonical passing gate {gate_id}")
        if "detail" in gate and (not isinstance(gate["detail"], str) or not gate["detail"]):
            finish("fail", f"actual certification lane gate {gate_id} has an invalid detail")

    waiver_audit = lane.get("waiver_audit")
    expected_waiver_fields = {
        "schema",
        "generated_at",
        "total_waivers",
        "active",
        "expired",
        "expiring_soon",
        "invalid",
        "waivers",
        "raw_waivers",
    }
    if not isinstance(waiver_audit, dict) or set(waiver_audit) != expected_waiver_fields:
        finish("fail", "actual certification lane waiver_audit fields are invalid")
    waiver_generated_raw = waiver_audit.get("generated_at")
    if (
        waiver_audit.get("schema") != "pi.ci.waiver_audit.v1"
        or not isinstance(waiver_generated_raw, str)
        or re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z", waiver_generated_raw) is None
    ):
        finish("fail", "actual certification lane waiver_audit schema or timestamp is invalid")
    try:
        waiver_generated = datetime.fromisoformat(waiver_generated_raw.removesuffix("Z") + "+00:00")
    except ValueError:
        finish("fail", "actual certification lane waiver_audit timestamp is invalid")
    if waiver_generated > now + timedelta(minutes=5) or now - waiver_generated > max_evidence_age:
        finish("fail", "actual certification lane waiver_audit is not fresh")
    if waiver_generated > lane_generated or (lane_generated - waiver_generated) > timedelta(minutes=5):
        finish("fail", "certification lane and waiver_audit timestamps differ by more than five minutes")
    waiver_count_fields = ("total_waivers", "active", "expired", "expiring_soon", "invalid")
    if any(
        isinstance(waiver_audit.get(field), bool)
        or not isinstance(waiver_audit.get(field), int)
        or waiver_audit.get(field) != 0
        for field in waiver_count_fields
    ):
        finish("fail", "strict drop-in certification may not rely on waivers")
    if waiver_audit.get("waivers") != [] or waiver_audit.get("raw_waivers") != [] or lane.get("waivers_applied") != []:
        finish("fail", "strict drop-in certification requires empty waiver inventories")

    expected_summary = {
        "total_gates": 20,
        "passed": 20,
        "failed": 0,
        "warned": 0,
        "skipped": 0,
        "waived": 0,
        "blocking_pass": 14,
        "blocking_total": 14,
        "all_blocking_pass": True,
    }
    summary = lane.get("summary")
    if isinstance(summary, dict):
        for field in (
            "total_gates",
            "passed",
            "failed",
            "warned",
            "skipped",
            "waived",
            "blocking_pass",
            "blocking_total",
        ):
            if isinstance(summary.get(field), bool) or not isinstance(summary.get(field), int):
                finish("fail", f"actual certification lane summary.{field} must be an integer")
        if not isinstance(summary.get("all_blocking_pass"), bool):
            finish("fail", "actual certification lane summary.all_blocking_pass must be boolean")
    if summary != expected_summary:
        finish("fail", "actual certification lane summary does not describe 20 canonical passing gates")
    expected_promotion = {
        "can_promote": True,
        "blocker_gates": [],
        "waiver_gates": [],
        "conditions": ["All blocking gates pass (including waivers)"],
    }
    promotion_rules = lane.get("promotion_rules")
    if not isinstance(promotion_rules, dict) or not isinstance(
        promotion_rules.get("can_promote"), bool
    ):
        finish("fail", "actual certification lane promotion_rules.can_promote must be boolean")
    if promotion_rules != expected_promotion:
        finish("fail", "actual certification lane promotion_rules are not the canonical no-waiver pass state")
    expected_rerun = {
        "preflight_command": "cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact",
        "full_command": "cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact",
        "single_gate_template": "See reproduce_command field on each gate",
    }
    if lane.get("rerun_guidance") != expected_rerun:
        finish("fail", "actual certification lane rerun_guidance is not canonical")
    if lane.get("verdict") != "pass":
        finish(
            "fail",
            f"actual certification lane verdict={lane.get('verdict')!r} (expected 'pass')",
        )

evidence_index = verdict.get("evidence_index")
if strict_required or certification_claimed:
    if not isinstance(evidence_index, list) or not evidence_index:
        finish("fail", "evidence_index missing/empty in strict mode")
    evidence_paths = []
    for index, entry in enumerate(evidence_index):
        if not isinstance(entry, dict) or set(entry) != {"path", "exists"}:
            finish("fail", f"evidence_index[{index}] must contain exactly path and exists")
        rel_path = entry.get("path")
        canonical_path = canonical_repo_path(rel_path)
        if canonical_path is None:
            finish("fail", f"evidence_index path must be canonical and repository-relative: {rel_path!r}")
        if entry.get("exists") is not True:
            finish("fail", f"evidence_index marks required artifact missing: {rel_path}")
        evidence_paths.append(canonical_path.as_posix())
    if len(evidence_paths) != len(set(evidence_paths)):
        finish("fail", "evidence_index contains duplicate paths")

    required_artifact_paths = []
    seen_required_artifacts = set()
    for expected in expected_gate_specs:
        for artifact in expected["required_artifacts"]:
            if artifact not in seen_required_artifacts:
                seen_required_artifacts.add(artifact)
                required_artifact_paths.append(artifact)
    if evidence_paths != required_artifact_paths:
        finish("fail", "evidence_index must exactly match the deduplicated contract artifact order")

    root_resolved = project_root.resolve(strict=True)
    missing_paths = []
    non_regular_paths = []
    escaped_paths = []
    for path in evidence_paths:
        candidate = project_root / path
        if not candidate.exists() and not candidate.is_symlink():
            missing_paths.append(path)
            continue
        if candidate.is_symlink() or not candidate.is_file():
            non_regular_paths.append(path)
            continue
        try:
            candidate.resolve(strict=True).relative_to(root_resolved)
        except (OSError, RuntimeError, ValueError):
            escaped_paths.append(path)
    if missing_paths:
        finish("fail", "evidence_index paths missing on disk: " + ", ".join(missing_paths))
    if non_regular_paths:
        finish(
            "fail",
            "evidence_index paths must be regular non-symlink files: " + ", ".join(non_regular_paths),
        )
    if escaped_paths:
        finish("fail", "evidence_index paths resolve outside the repository: " + ", ".join(escaped_paths))

    provenance_paths = [
        "docs/contracts/dropin-certification-contract.json",
        "docs/evidence/dropin-certification-verdict.json",
        *evidence_paths,
    ]
    untracked_paths = []
    dirty_paths = []
    missing_from_head = []
    non_blob_paths = []
    for path in provenance_paths:
        candidate = project_root / path
        if candidate.is_symlink() or not candidate.is_file():
            non_regular_paths.append(path)

        head_entry = run_git("ls-tree", "-z", "HEAD", "--", path, text=False)
        if head_entry.returncode != 0:
            finish("fail", f"unable to inspect HEAD provenance for evidence path: {path}")
        records = [record for record in head_entry.stdout.split(b"\0") if record]
        if not records:
            missing_from_head.append(path)
            continue
        if len(records) != 1:
            non_blob_paths.append(path)
            continue
        try:
            metadata, recorded_path = records[0].split(b"\t", 1)
            mode, object_type, _object_id = metadata.split(b" ", 2)
        except ValueError:
            non_blob_paths.append(path)
            continue
        if (
            mode not in (b"100644", b"100755")
            or object_type != b"blob"
            or os.fsdecode(recorded_path) != path
        ):
            non_blob_paths.append(path)
            continue

        diff = run_git("diff", "--quiet", "HEAD", "--", path)
        if diff.returncode == 1:
            dirty_paths.append(path)
        elif diff.returncode != 0:
            finish("fail", f"unable to inspect worktree provenance for evidence path: {path}")

        untracked = run_git("ls-files", "--others", "--exclude-standard", "-z", "--", path, text=False)
        if untracked.returncode != 0:
            finish("fail", f"unable to inspect untracked evidence files under: {path}")
        if untracked.stdout:
            untracked_paths.append(path)

    if missing_from_head:
        finish("fail", "evidence paths are not tracked by release HEAD: " + ", ".join(missing_from_head))
    if non_regular_paths:
        finish(
            "fail",
            "release provenance paths must be regular non-symlink files: "
            + ", ".join(dict.fromkeys(non_regular_paths)),
        )
    if non_blob_paths:
        finish(
            "fail",
            "evidence paths must be canonical regular-file blobs in release HEAD: "
            + ", ".join(non_blob_paths),
        )
    if dirty_paths:
        finish("fail", "evidence paths differ from release HEAD: " + ", ".join(dirty_paths))
    if untracked_paths:
        finish("fail", "evidence paths contain untracked files: " + ", ".join(untracked_paths))

if strict_required or certification_claimed:
    finish_verified("pass", "strict drop-in certification verdict is CERTIFIED with complete hard-gate evidence")
else:
    finish_verified("warn", f"release-source drop-in verdict is not certified (overall_verdict={overall}; strict drop-in mode disabled)")
PY
); then
    :
else
    DROPIN_CHECK="fail|unexpected drop-in verdict validator error: $DROPIN_CHECK"
fi

DROPIN_STATUS="${DROPIN_CHECK%%|*}"
DROPIN_DETAIL="${DROPIN_CHECK#*|}"
case "$DROPIN_STATUS" in
    pass)
        check_pass "dropin_verdict" "$DROPIN_DETAIL"
        ;;
    warn)
        check_warn "dropin_verdict" "$DROPIN_DETAIL"
        ;;
    fail)
        check_fail "dropin_verdict" "$DROPIN_DETAIL"
        ;;
    *)
        check_fail "dropin_verdict" "unexpected drop-in verdict validation result: $DROPIN_CHECK"
        ;;
esac

# Gate 15: Re-capture the same raw-byte repository fingerprint after every
# executable gate. This detects HEAD/index changes, special index flags,
# symlink substitution, untracked files, and worktree modifications hidden by
# clean/smudge filters.
if FINAL_REPOSITORY_SNAPSHOT=$(capture_repository_snapshot 2>&1); then
    if [[ -n "$INITIAL_REPOSITORY_SNAPSHOT" && "$FINAL_REPOSITORY_SNAPSHOT" == "$INITIAL_REPOSITORY_SNAPSHOT" ]]; then
        check_pass "final_repository_state" "HEAD, canonical tree, index, flags, symlinks, untracked paths, and raw worktree bytes remained unchanged"
    elif [[ -z "$INITIAL_REPOSITORY_SNAPSHOT" ]]; then
        check_fail "final_repository_state" "Final source is clean, but no valid initial repository fingerprint was captured"
    else
        check_fail "final_repository_state" "Repository fingerprint changed during gate execution"
    fi
else
    check_fail "final_repository_state" "Repository source is not byte-for-byte clean after gate execution: $FINAL_REPOSITORY_SNAPSHOT"
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
    "max_na_count": $MAX_NA_COUNT,
    "max_evidence_age_hours": $MAX_EVIDENCE_AGE_HOURS,
    "require_dropin_certified": $REQUIRE_DROPIN_CERTIFIED,
    "require_performance_claim_ready": $REQUIRE_PERFORMANCE_CLAIM_READY,
    "require_preflight": $REQUIRE_PREFLIGHT,
    "require_quality": $REQUIRE_QUALITY
  },
  "cargo_runner": {
    "requested": "$CARGO_RUNNER_REQUEST",
    "resolved": "$CARGO_RUNNER_MODE"
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
    if [[ $FAIL_COUNT -gt 0 ]]; then
        exit 1
    fi
else
    echo ""
    echo "═══════════════════════════════════════════════════════════"
    echo "  Release Gate — Conformance Evidence Bundle"
    echo "═══════════════════════════════════════════════════════════"
    echo "  Pass: $PASS_COUNT  Fail: $FAIL_COUNT  Warn: $WARN_COUNT  Total: $TOTAL_CHECKS"
    echo "  Thresholds: pass_rate>=${MIN_PASS_RATE}%, fail<=${MAX_FAIL_COUNT}, na<=${MAX_NA_COUNT}, evidence_age<=${MAX_EVIDENCE_AGE_HOURS}h, performance_claim_ready_required=${REQUIRE_PERFORMANCE_CLAIM_READY}"
    echo "═══════════════════════════════════════════════════════════"

    if [[ $FAIL_COUNT -gt 0 ]]; then
        echo "  VERDICT: FAIL — release blocked"
        exit 1
    else
        echo "  VERDICT: PASS — release approved"
    fi
fi
