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

for toggle_name in REQUIRE_DROPIN_CERTIFIED REQUIRE_PREFLIGHT REQUIRE_QUALITY; do
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

capture_repository_snapshot() {
    python3 - "$PROJECT_ROOT" <<'PY'
import hashlib
import os
import stat
import subprocess
import sys
from pathlib import Path

root = Path(sys.argv[1])


def fail(detail):
    print(detail, file=sys.stderr)
    raise SystemExit(1)


def git(*args):
    env = os.environ.copy()
    env["GIT_LITERAL_PATHSPECS"] = "1"
    result = subprocess.run(
        ["git", "-C", str(root), *args],
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


if root.is_symlink() or not root.is_dir():
    fail("repository root must be a real directory, not a symlink")

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
        try:
            with open(full_path, "rb") as handle:
                contents = handle.read()
        except OSError as exc:
            fail(f"cannot read tracked path {os.fsdecode(path)!r}: {exc}")
    else:
        if not stat.S_ISLNK(file_stat.st_mode):
            fail(f"tracked symlink has wrong worktree type: {os.fsdecode(path)!r}")
        try:
            contents = os.readlink(full_path)
        except OSError as exc:
            fail(f"cannot read tracked symlink {os.fsdecode(path)!r}: {exc}")

    framed = b"blob " + str(len(contents)).encode("ascii") + b"\0" + contents
    if len(expected_oid) == 40:
        actual_oid = hashlib.sha1(framed).hexdigest().encode("ascii")
    elif len(expected_oid) == 64:
        actual_oid = hashlib.sha256(framed).hexdigest().encode("ascii")
    else:
        fail(f"unsupported Git object ID length for {os.fsdecode(path)!r}")
    if actual_oid != expected_oid:
        fail(f"raw worktree bytes differ from release HEAD at {os.fsdecode(path)!r}")

if git("rev-parse", "--verify", "HEAD^{commit}").decode("ascii", "strict").strip() != head:
    fail("HEAD changed while repository state was captured")
if git("ls-files", "--stage", "-z") != index_bytes:
    fail("index changed while repository state was captured")
if git("ls-files", "-v", "-z") != flag_bytes:
    fail("index flags changed while repository state was captured")
if git("ls-files", "--others", "--exclude-standard", "-z") != untracked_bytes:
    fail("untracked path set changed while repository state was captured")

print(f"{head}|{tree_digest}")
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
    if EVIDENCE_CHECK=$(python3 - "$PROJECT_ROOT" "$EVIDENCE_DIR" 2>&1 <<'PY'
import fnmatch
import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

project_root = Path(sys.argv[1])
evidence_dir = Path(sys.argv[2])

def finish(status, detail):
    print(f"{status}|{detail}")
    raise SystemExit(0)

def load_object(path, label):
    if path.is_symlink():
        finish("fail", f"{label} must be a regular file, not a symlink: {path}")
    if not path.is_file():
        finish("fail", f"{label} is missing: {path}")
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001
        finish("fail", f"{label} is not valid JSON: {exc}")
    if not isinstance(payload, dict):
        finish("fail", f"{label} root must be an object")
    return payload

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
    env = os.environ.copy()
    env["GIT_LITERAL_PATHSPECS"] = "1"
    return subprocess.run(
        ["git", "-C", str(project_root), *args],
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
decision_paths = [contract_path, environment_path, summary_path]
contract = load_object(contract_path, "evidence contract")
environment = load_object(environment_path, "E2E environment")
summary = load_object(summary_path, "E2E summary")

if contract.get("schema") != "pi.evidence.contract.v1":
    finish("fail", f"unsupported evidence contract schema: {contract.get('schema')!r}")
if environment.get("schema") != "pi.e2e.environment.v1":
    finish("fail", f"unsupported E2E environment schema: {environment.get('schema')!r}")
if summary.get("schema") != "pi.e2e.summary.v1":
    finish("fail", f"unsupported E2E summary schema: {summary.get('schema')!r}")

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

if contract.get("status") != "pass":
    finish("fail", f"evidence contract status={contract.get('status')!r} (expected 'pass')")
errors = contract.get("errors")
if not isinstance(errors, list) or errors:
    finish("fail", "evidence contract errors must be an empty array")
checks = contract.get("checks")
if not isinstance(checks, list) or not checks:
    finish("fail", "evidence contract checks must be a non-empty array")
seen_check_ids = set()
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
environment_units = string_list(environment.get("unit_targets"), "environment.unit_targets", nonempty=True)
observed_unit_names = []
for index, embedded_result in enumerate(unit_results):
    if not isinstance(embedded_result, dict):
        finish("fail", f"summary.unit_targets[{index}] must be an object")
    target_name = embedded_result.get("target")
    if not isinstance(target_name, str) or re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]*", target_name) is None:
        finish("fail", f"summary.unit_targets[{index}] has an unsafe target name")
    observed_unit_names.append(target_name)
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
    decision_paths.append(result_path)
    actual_result = load_object(result_path, f"E2E result for {suite_name}")
    for field in ("suite", "exit_code", "passed", "failed", "ignored", "total"):
        if actual_result.get(field) != embedded_result.get(field):
            finish("fail", f"summary and result.json disagree for {suite_name}.{field}")
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

source_commit = environment.get("git_sha")
if not isinstance(source_commit, str) or re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", source_commit) is None:
    finish("fail", "environment.git_sha must be a full lowercase Git commit ID")
source_check = run_git("rev-parse", "--verify", f"{source_commit}^{{commit}}")
if source_check.returncode != 0 or source_check.stdout.strip() != source_commit:
    finish("fail", f"environment.git_sha does not resolve exactly to a commit: {source_commit}")
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

for decision_path in decision_paths:
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
    if committed.stdout != worktree_bytes:
        finish("fail", f"E2E decision input bytes differ from release HEAD: {relative}")
    diff = run_git("diff", "--quiet", "HEAD", "--", relative)
    if diff.returncode == 1:
        finish("fail", f"E2E decision input index/worktree differs from release HEAD: {relative}")
    if diff.returncode != 0:
        finish("fail", f"unable to inspect E2E decision input state: {relative}")

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

root = Path(sys.argv[1])
summary_path = Path(sys.argv[2])
minimum_rate = int(sys.argv[3])
maximum_age = timedelta(hours=int(sys.argv[4]))


def git(*args):
    env = os.environ.copy()
    env["GIT_LITERAL_PATHSPECS"] = "1"
    result = subprocess.run(
        ["git", "-C", str(root), *args],
        capture_output=True,
        env=env,
        check=False,
    )
    if result.returncode != 0:
        diagnostic = result.stderr.decode("utf-8", "replace").strip()
        raise ValueError(f"git {' '.join(args)} failed: {diagnostic}")
    return result.stdout


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
if summary_path.is_symlink() or not summary_path.is_file():
    raise ValueError("conformance summary must be a regular non-symlink file")
current = root
for part in Path(summary_relative).parts[:-1]:
    current /= part
    metadata = os.lstat(current)
    if stat.S_ISLNK(metadata.st_mode):
        raise ValueError(f"conformance summary traverses symlinked parent: {current}")

head = git("rev-parse", "--verify", "HEAD^{commit}").decode("ascii", "strict").strip()
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

data = json.loads(raw_summary)
if not isinstance(data, dict):
    raise ValueError("summary root must be an object")
if data.get("schema") != "pi.ext.conformance_summary.v2":
    raise ValueError(f"unsupported conformance summary schema: {data.get('schema')!r}")

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
ancestor = subprocess.run(
    ["git", "-C", str(root), "merge-base", "--is-ancestor", source_commit, head],
    capture_output=True,
    check=False,
)
if ancestor.returncode == 1:
    raise ValueError("conformance source commit is not an ancestor of release HEAD")
if ancestor.returncode != 0:
    raise ValueError("unable to verify conformance source ancestry")

source_tree = git("ls-tree", "-r", "-z", "--full-tree", source_commit)
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

changed_paths = [os.fsdecode(path) for path in git("diff", "--name-only", "-z", source_commit, head).split(b"\0") if path]
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

counts = data.get('counts', {})
if not isinstance(counts, dict):
    raise ValueError("counts must be an object")

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
if "tested" in counts and counts["tested"] != tested:
    raise ValueError("counts.tested must equal counts.pass + counts.fail")

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

rate_display = format(float(pass_rate), ".12g")
rate_passes = int(float(pass_rate) >= minimum_rate)
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

# Gate 6: Compilation check (cargo check)
if run_cargo_gate check --locked --lib --quiet 2>/dev/null; then
    check_pass "cargo_check" "Library compiles cleanly"
else
    check_fail "cargo_check" "cargo check --lib failed"
fi

# Gate 7: Clippy lint
if run_cargo_gate clippy --locked --lib --quiet -- -D warnings 2>/dev/null; then
    check_pass "clippy" "No clippy warnings"
else
    check_fail "clippy" "Clippy has warnings"
fi

# Gate 8: Preflight analyzer (optional)
if [[ "$REQUIRE_PREFLIGHT" -eq 1 ]]; then
    if run_cargo_gate test --locked --lib extension_preflight --quiet 2>/dev/null; then
        check_pass "preflight_tests" "Extension preflight tests pass"
    else
        check_fail "preflight_tests" "Extension preflight tests failed"
    fi
fi

# Gate 9: Quality pipeline (optional)
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

# Gate 12: Drop-in certification contract artifact
DROPIN_CONTRACT="$PROJECT_ROOT/docs/contracts/dropin-certification-contract.json"
if [[ -f "$DROPIN_CONTRACT" ]]; then
    if CONTRACT_CHECK=$(python3 - "$DROPIN_CONTRACT" 2>&1 <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
try:
    data = json.loads(path.read_text(encoding="utf-8"))
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

# Gate 13: Drop-in certification verdict (required for strict claim mode)
DROPIN_VERDICT="$PROJECT_ROOT/docs/evidence/dropin-certification-verdict.json"
if DROPIN_CHECK=$(python3 - "$PROJECT_ROOT" "$DROPIN_CONTRACT" "$DROPIN_VERDICT" "$REQUIRE_DROPIN_CERTIFIED" 2>&1 <<'PY'
import fnmatch
import json
import os
import re
import subprocess
import sys
import tomllib
from datetime import datetime
from pathlib import Path, PurePosixPath

project_root = Path(sys.argv[1])
contract_path = Path(sys.argv[2])
verdict_path = Path(sys.argv[3])
# IMPORTANT: this must track the shell-resolved gate toggle derived from
# RELEASE_GATE_REQUIRE_DROPIN_CERTIFIED. Reading an unrelated env var here
# can silently disable strict drop-in enforcement.
strict_required = sys.argv[4] == "1"
certification_claimed = False

def finish(status, detail):
    print(f"{status}|{detail}")
    raise SystemExit(0)

def provenance_failure(detail):
    if strict_required or certification_claimed:
        finish("fail", detail)
    finish("warn", f"{detail} (strict drop-in mode disabled)")

def run_git(*args, text=True):
    env = os.environ.copy()
    env["GIT_LITERAL_PATHSPECS"] = "1"
    return subprocess.run(
        ["git", "-C", str(project_root), *args],
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

if contract_path.is_symlink() or not contract_path.is_file():
    finish("fail", "contract must be a regular non-symlink file")

try:
    contract = json.loads(contract_path.read_text(encoding="utf-8"))
except Exception as exc:  # noqa: BLE001
    finish("fail", f"contract parse error: {exc}")

if not isinstance(contract, dict):
    finish("fail", "contract root must be an object")

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

if verdict_path.is_symlink() or not verdict_path.is_file():
    if strict_required:
        finish(
            "fail",
            "docs/evidence/dropin-certification-verdict.json must be a regular non-symlink file in strict drop-in mode",
        )
    else:
        finish(
            "warn",
            "docs/evidence/dropin-certification-verdict.json is absent or not a regular non-symlink file "
            "(strict drop-in mode disabled)",
        )

try:
    verdict = json.loads(verdict_path.read_text(encoding="utf-8"))
except Exception as exc:  # noqa: BLE001
    finish("fail", f"verdict parse error: {exc}")

if not isinstance(verdict, dict):
    finish("fail", "verdict root must be an object")

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

verdict_commit = verdict.get("git_commit")
if not isinstance(verdict_commit, str) or re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", verdict_commit) is None:
    finish("fail", "git_commit must be a full lowercase Git object ID")

commit_check = run_git("rev-parse", "--verify", f"{verdict_commit}^{{commit}}")
if commit_check.returncode != 0:
    finish("fail", f"git_commit={verdict_commit} is not a commit in this repository")
if commit_check.stdout.strip() != verdict_commit:
    finish("fail", f"git_commit={verdict_commit} did not resolve exactly")

head_check = run_git("rev-parse", "--verify", "HEAD^{commit}")
if head_check.returncode != 0:
    finish("fail", "unable to resolve the current release HEAD")
current_head = head_check.stdout.strip()

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

    generated_at = verdict.get("generated_at_utc")
    if not isinstance(generated_at, str) or not generated_at.endswith("Z"):
        finish("fail", "generated_at_utc must be an RFC3339 UTC timestamp ending in Z")
    try:
        datetime.fromisoformat(generated_at.removesuffix("Z") + "+00:00")
    except ValueError:
        finish("fail", "generated_at_utc must be a valid RFC3339 UTC timestamp")

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
    finish("pass", "strict drop-in certification verdict is CERTIFIED with complete hard-gate evidence")
else:
    finish("warn", f"release-source drop-in verdict is not certified (overall_verdict={overall}; strict drop-in mode disabled)")
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

# Gate 14: Re-capture the same raw-byte repository fingerprint after every
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
    echo "  Thresholds: pass_rate>=${MIN_PASS_RATE}%, fail<=${MAX_FAIL_COUNT}, na<=${MAX_NA_COUNT}, evidence_age<=${MAX_EVIDENCE_AGE_HOURS}h"
    echo "═══════════════════════════════════════════════════════════"

    if [[ $FAIL_COUNT -gt 0 ]]; then
        echo "  VERDICT: FAIL — release blocked"
        exit 1
    else
        echo "  VERDICT: PASS — release approved"
    fi
fi
