#!/usr/bin/env bash
# scripts/e2e/run_all.sh — Unified verification runner with structured artifacts.
#
# Runs lint gates, inline lib tests, integration test targets, and E2E suites
# with per-target artifact collection. Targets auto-discovered from
# tests/suite_classification.toml.
#
# Usage:
#   ./scripts/e2e/run_all.sh                              # profile=full (lint + lib + all targets)
#   ./scripts/e2e/run_all.sh --profile quick              # fast local loop (lint + lib + unit only)
#   ./scripts/e2e/run_all.sh --profile focused            # lint + lib + selected integration targets
#   ./scripts/e2e/run_all.sh --profile ci                 # deterministic CI (lint + lib + all non-e2e + 1 e2e)
#   ./scripts/e2e/run_all.sh --suite e2e_tui              # run specific E2E suite(s)
#   ./scripts/e2e/run_all.sh --unit-target node_http_shim # run specific integration target(s)
#   ./scripts/e2e/run_all.sh --rerun-from <summary.json>  # deterministic rerun of failed suites
#   ./scripts/e2e/run_all.sh --diff-from <summary.json>   # compare current run to baseline
#   ./scripts/e2e/run_all.sh --skip-lint                  # skip format/clippy gates
#   ./scripts/e2e/run_all.sh --list                       # list available suites
#   ./scripts/e2e/run_all.sh --list-profiles              # list built-in profiles
#
# Environment:
#   E2E_ARTIFACT_DIR   Override artifact output directory (default: tests/e2e_results/<timestamp>)
#   E2E_PARALLELISM    Cargo test threads (default: 1 for determinism)
#   RUST_LOG           Log level for test harness (default: info)
#   VCR_MODE           Override VCR mode for all suites (default: unset, per-test decision)
#   VERIFY_PROFILE     Default profile when --profile is omitted (default: full)
#   E2E_DIFF_FROM      Baseline summary.json to diff against (optional)
#   VERIFY_MIN_FREE_MB Minimum free MB required for repo/artifact mounts (default: 2048)
#   VERIFY_MIN_TMP_FREE_MB Minimum free MB required for tmp/cargo mounts (default: 8192)
#   CARGO_TARGET_DIR   Optional cargo target directory (checked by preflight).
#                      If unset and CODEX_THREAD_ID is present, defaults to
#                      target/agents/<CODEX_THREAD_ID> to reduce multi-agent
#                      artifact contention.
#   CARGO_HOME         Optional cargo home directory (checked by preflight)
#   VERIFY_CARGO_RUNNER Cargo execution mode: auto | rch | local (default: auto)
#   VERIFY_MIN_FREE_INODE_PCT Minimum free inode percent required (default: 5)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

# In multi-agent Codex sessions, isolate cargo artifacts by default unless the
# caller explicitly sets CARGO_TARGET_DIR.
if [[ -z "${CARGO_TARGET_DIR:-}" && -n "${CODEX_THREAD_ID:-}" ]]; then
    safe_codex_thread_id="$(printf '%s' "$CODEX_THREAD_ID" | tr -c 'A-Za-z0-9._-' '_')"
    export CARGO_TARGET_DIR="$PROJECT_ROOT/target/agents/$safe_codex_thread_id"
fi

# ─── Configuration ────────────────────────────────────────────────────────────

if [[ -n "${E2E_ARTIFACT_DIR+x}" ]]; then
    ARTIFACT_DIR="$E2E_ARTIFACT_DIR"
    if ! artifact_timestamp_values="$(python3 - "$ARTIFACT_DIR" <<'PY'
import re
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

artifact_dir = Path(sys.argv[1])
timestamp = artifact_dir.name
if re.fullmatch(r"[0-9]{8}T[0-9]{6}Z", timestamp) is None:
    raise SystemExit(
        "E2E_ARTIFACT_DIR basename must use canonical YYYYMMDDTHHMMSSZ syntax"
    )
try:
    instant = datetime.strptime(timestamp, "%Y%m%dT%H%M%SZ").replace(
        tzinfo=timezone.utc
    )
except ValueError as exc:
    raise SystemExit(f"E2E_ARTIFACT_DIR basename is not a real UTC instant: {exc}") from exc
if instant > datetime.now(timezone.utc) + timedelta(minutes=5):
    raise SystemExit("E2E_ARTIFACT_DIR timestamp is more than five minutes in the future")
print(f"{instant:%Y-%m-%dT%H:%M:%SZ}|{timestamp}")
PY
)"; then
        echo "Invalid E2E_ARTIFACT_DIR: $ARTIFACT_DIR" >&2
        exit 1
    fi
    IFS='|' read -r GENERATED_AT TIMESTAMP extra_timestamp_field <<<"$artifact_timestamp_values"
    if [[ -n "$extra_timestamp_field" ]] || \
        [[ ! "$GENERATED_AT" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] || \
        [[ ! "$TIMESTAMP" =~ ^[0-9]{8}T[0-9]{6}Z$ ]]; then
        echo "Invalid E2E_ARTIFACT_DIR timestamp binding: $ARTIFACT_DIR" >&2
        exit 1
    fi
else
    GENERATED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    TIMESTAMP="${GENERATED_AT//-/}"
    TIMESTAMP="${TIMESTAMP//:/}"
    ARTIFACT_DIR="$PROJECT_ROOT/tests/e2e_results/$TIMESTAMP"
fi
PARALLELISM="${E2E_PARALLELISM:-1}"
LOG_LEVEL="${RUST_LOG:-info}"
PROFILE="${VERIFY_PROFILE:-full}"
RERUN_FROM=""
DIFF_FROM="${E2E_DIFF_FROM:-}"
SHARD_KIND="none"
SHARD_INDEX_RAW=""
SHARD_TOTAL_RAW=""
SHARD_INDEX_JSON="null"
SHARD_TOTAL_JSON="null"
SHARD_NAME=""
SHARD_INDEX=-1
SHARD_TOTAL=1
CORRELATION_ID="${CI_CORRELATION_ID:-}"
CARGO_RUNNER_MODE="${VERIFY_CARGO_RUNNER:-auto}"
CARGO_RUNNER_DESC="cargo"
CARGO_EXEC_PREFIX=""
CARGO_EXEC_PREFIX_JSON="[]"
CARGO_RUNNER_ARGS=()
SOURCE_COMMIT=""
SOURCE_SNAPSHOT=""

configure_cargo_runner() {
    case "$CARGO_RUNNER_MODE" in
        auto)
            if command -v rch >/dev/null 2>&1; then
                CARGO_RUNNER_ARGS=("rch" "exec" "--")
            else
                CARGO_RUNNER_ARGS=()
            fi
            ;;
        rch)
            if ! command -v rch >/dev/null 2>&1; then
                echo "VERIFY_CARGO_RUNNER=rch requested, but 'rch' is not available in PATH." >&2
                exit 1
            fi
            CARGO_RUNNER_ARGS=("rch" "exec" "--")
            ;;
        local)
            CARGO_RUNNER_ARGS=()
            ;;
        *)
            echo "Unknown VERIFY_CARGO_RUNNER value: $CARGO_RUNNER_MODE (expected: auto|rch|local)" >&2
            exit 1
            ;;
    esac

    if [[ ${#CARGO_RUNNER_ARGS[@]} -gt 0 ]]; then
        CARGO_RUNNER_DESC="${CARGO_RUNNER_ARGS[*]} cargo"
        CARGO_EXEC_PREFIX="${CARGO_RUNNER_ARGS[*]}"
        CARGO_EXEC_PREFIX_JSON="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1:]))' "${CARGO_RUNNER_ARGS[@]}" 2>/dev/null || echo '[]')"
    else
        CARGO_RUNNER_DESC="cargo"
        CARGO_EXEC_PREFIX=""
        CARGO_EXEC_PREFIX_JSON="[]"
    fi
}

run_cargo() {
    if [[ ${#CARGO_RUNNER_ARGS[@]} -gt 0 ]]; then
        local env_overrides=()
        local remote_target_dir remote_tmpdir

        if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
            remote_target_dir="$(path_for_remote_runner "$CARGO_TARGET_DIR")"
            if [[ "$remote_target_dir" != "$CARGO_TARGET_DIR" ]]; then
                env_overrides+=("CARGO_TARGET_DIR=$remote_target_dir")
            fi
        fi
        if [[ -n "${TMPDIR:-}" ]]; then
            remote_tmpdir="$(path_for_remote_runner "$TMPDIR")"
            if [[ "$remote_tmpdir" != "$TMPDIR" ]]; then
                env_overrides+=("TMPDIR=$remote_tmpdir")
            fi
        fi

        if [[ ${#env_overrides[@]} -gt 0 ]]; then
            env "RCH_FORCE_REMOTE=${RCH_FORCE_REMOTE:-true}" "${CARGO_RUNNER_ARGS[@]}" env "${env_overrides[@]}" cargo "$@"
        else
            env "RCH_FORCE_REMOTE=${RCH_FORCE_REMOTE:-true}" "${CARGO_RUNNER_ARGS[@]}" cargo "$@"
        fi
    else
        cargo "$@"
    fi
}

path_for_remote_runner() {
    local original="$1"
    if [[ "$original" == "$PROJECT_ROOT/"* ]]; then
        printf '%s' "${original#"$PROJECT_ROOT"/}"
    else
        printf '%s' "$original"
    fi
}

path_for_test_harness() {
    local original="$1"
    if [[ ${#CARGO_RUNNER_ARGS[@]} -gt 0 ]]; then
        path_for_remote_runner "$original"
    else
        printf '%s' "$original"
    fi
}

# ─── Auto-discover targets from suite_classification.toml ─────────────────────

CLASSIFICATION_FILE="$PROJECT_ROOT/tests/suite_classification.toml"

read_toml_array() {
    local suite_name="$1"
    python3 -c "
import tomllib, sys
with open('$CLASSIFICATION_FILE', 'rb') as f:
    data = tomllib.load(f)
for name in data.get('suite', {}).get('$suite_name', {}).get('files', []):
    print(name)
" 2>/dev/null || true
}

mapfile -t ALL_UNIT_FILES < <(read_toml_array "unit")
mapfile -t ALL_VCR_FILES < <(read_toml_array "vcr")
mapfile -t ALL_E2E_FILES < <(read_toml_array "e2e")

if [[ ${#ALL_UNIT_FILES[@]} -eq 0 || ${#ALL_VCR_FILES[@]} -eq 0 || ${#ALL_E2E_FILES[@]} -eq 0 ]]; then
    echo "Failed to load non-empty unit, vcr, and e2e suites from $CLASSIFICATION_FILE" >&2
    exit 1
fi

# Combined non-E2E targets (unit + vcr) for integration test phase.
ALL_UNIT_TARGETS=("${ALL_UNIT_FILES[@]}" "${ALL_VCR_FILES[@]}")

# E2E suites.
ALL_SUITES=("${ALL_E2E_FILES[@]}")

# ─── CLI Parsing ──────────────────────────────────────────────────────────────

SELECTED_SUITES=()
SELECTED_UNIT_TARGETS=()
LIST_ONLY=false
LIST_PROFILES=false
SKIP_UNIT=false
SKIP_E2E=false
SKIP_LINT=false

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
        --diff-from)
            shift
            DIFF_FROM="$1"
            shift
            ;;
        --skip-unit)
            SKIP_UNIT=true
            shift
            ;;
        --skip-e2e)
            SKIP_E2E=true
            shift
            ;;
        --skip-lint)
            SKIP_LINT=true
            shift
            ;;
        --shard-kind)
            shift
            SHARD_KIND="$1"
            shift
            ;;
        --shard-index)
            shift
            SHARD_INDEX_RAW="$1"
            shift
            ;;
        --shard-total)
            shift
            SHARD_TOTAL_RAW="$1"
            shift
            ;;
        --shard-name)
            shift
            SHARD_NAME="$1"
            shift
            ;;
        --correlation-id)
            shift
            CORRELATION_ID="$1"
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
            echo "          [--rerun-from SUMMARY_JSON] [--diff-from SUMMARY_JSON]"
            echo "          [--skip-unit] [--skip-e2e] [--skip-lint]"
            echo "          [--shard-kind KIND --shard-index N --shard-total M]"
            echo "          [--list] [--list-profiles] [--help]"
            echo ""
            echo "Options:"
            echo "  --profile NAME       Verification profile: quick | focused | ci | full"
            echo "  --suite NAME         Run only specified E2E suite(s) (repeatable)"
            echo "  --unit-target NAME   Run only specified unit target(s) (repeatable)"
            echo "  --rerun-from PATH    Rerun failed suites from prior summary.json"
            echo "  --diff-from PATH     Compare current run against baseline summary.json"
            echo "  --skip-unit          Skip integration target execution"
            echo "  --skip-e2e           Skip E2E suite execution"
            echo "  --skip-lint          Skip fmt/clippy lint gates"
            echo "  --shard-kind KIND    Deterministic shard mode: none|unit|suite|both"
            echo "  --shard-index N      Zero-based shard index"
            echo "  --shard-total M      Total shard count"
            echo "  --shard-name NAME    Optional shard label (default derived from kind/index/total)"
            echo "  --correlation-id ID  Correlation id written into every artifact manifest"
            echo "  --list               List available E2E suites and exit"
            echo "  --list-profiles      List available verification profiles and exit"
            echo "  --help               Show this help"
            echo ""
            echo "Profiles:"
            echo "  quick    Lint + lib inline tests + unit suite only (fastest)"
            echo "  focused  Lint + lib + unit + selected integration targets"
            echo "  ci       Lint + lib + all non-E2E targets + 1 E2E suite"
            echo "  full     Lint + lib + all targets + all E2E suites"
            echo ""
            echo "Environment:"
            echo "  E2E_ARTIFACT_DIR     Artifact output directory"
            echo "  E2E_PARALLELISM      Cargo test threads (default: 1)"
            echo "  RUST_LOG             Log level (default: info)"
            echo "  VERIFY_PROFILE       Default profile when --profile not provided"
            echo "  E2E_DIFF_FROM        Baseline summary.json for diff/triage output"
            echo "  VERIFY_MIN_TMP_FREE_MB Minimum free MB for tmp/cargo mounts (default: 8192)"
            echo "  CARGO_TARGET_DIR     Optional cargo target directory (checked by preflight); defaults to target/agents/<CODEX_THREAD_ID> when CODEX_THREAD_ID is set"
            echo "  CARGO_HOME           Optional cargo home directory (checked by preflight)"
            echo "  VERIFY_CARGO_RUNNER  Cargo execution mode: auto|rch|local (default: auto)"
            echo "  VERIFY_MIN_FREE_MB   Minimum free MB for repo/artifact mounts (default: 2048)"
            echo "  VERIFY_MIN_FREE_INODE_PCT Minimum free inode percent required (default: 5)"
            echo "  CI_CORRELATION_ID    Default correlation id when --correlation-id is not provided"
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 1
            ;;
    esac
done

configure_cargo_runner

if $LIST_PROFILES; then
    cat <<EOF
Available verification profiles:
  quick:   Lint + lib inline tests + unit suite targets only (${#ALL_UNIT_FILES[@]} targets)
  focused: Lint + lib + unit + selected integration targets + 2 E2E suites
  ci:      Lint + lib + all non-E2E targets (${#ALL_UNIT_TARGETS[@]}) + 1 E2E suite
  full:    Lint + lib + all targets (${#ALL_UNIT_TARGETS[@]} + ${#ALL_SUITES[@]} E2E)
EOF
    exit 0
fi

if $LIST_ONLY; then
    echo "Available E2E suites (${#ALL_SUITES[@]}):"
    for suite in "${ALL_SUITES[@]}"; do
        if [[ -f "tests/${suite}.rs" ]]; then
            echo "  $suite"
        else
            echo "  $suite (missing)"
        fi
    done
    echo ""
    echo "Non-E2E targets (${#ALL_UNIT_TARGETS[@]}):"
    echo "  Unit suite (${#ALL_UNIT_FILES[@]}): ${ALL_UNIT_FILES[*]:0:5}..."
    echo "  VCR suite (${#ALL_VCR_FILES[@]}): ${ALL_VCR_FILES[*]:0:5}..."
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
        quick)
            SELECTED_SUITES=()
            ;;
        *)
            echo "Unknown --profile value: $PROFILE (expected: quick|focused|ci|full)" >&2
            exit 1
            ;;
    esac
fi

if [[ ${#SELECTED_UNIT_TARGETS[@]} -eq 0 && "$SKIP_UNIT" == false ]]; then
    case "$PROFILE" in
        full|ci)
            SELECTED_UNIT_TARGETS=("${ALL_UNIT_TARGETS[@]}")
            ;;
        quick)
            SELECTED_UNIT_TARGETS=("${ALL_UNIT_FILES[@]}")
            ;;
        focused)
            SELECTED_UNIT_TARGETS=(
                "${ALL_UNIT_FILES[@]}"
                ext_conformance_matrix
                node_buffer_shim
                node_bun_api_matrix
                node_crypto_shim
                node_http_shim
                npm_module_stubs
                extensions_registration
                extensions_policy_negative
            )
            ;;
    esac
fi

if [[ "$SKIP_UNIT" == true ]]; then
    SELECTED_UNIT_TARGETS=()
fi

if [[ "$SKIP_E2E" == true ]]; then
    SELECTED_SUITES=()
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

if [[ -z "$DIFF_FROM" && -n "$RERUN_FROM" ]]; then
    DIFF_FROM="$RERUN_FROM"
fi

if [[ -n "$DIFF_FROM" && ! -f "$DIFF_FROM" ]]; then
    echo "Diff baseline summary not found: $DIFF_FROM" >&2
    exit 1
fi

if [[ -n "$SHARD_INDEX_RAW" || -n "$SHARD_TOTAL_RAW" ]]; then
    if [[ -z "$SHARD_INDEX_RAW" || -z "$SHARD_TOTAL_RAW" ]]; then
        echo "Both --shard-index and --shard-total are required when sharding is enabled." >&2
        exit 1
    fi
fi

if [[ ! "$SHARD_KIND" =~ ^(none|unit|suite|both)$ ]]; then
    echo "Invalid --shard-kind value: $SHARD_KIND (expected none|unit|suite|both)" >&2
    exit 1
fi

if [[ "$SHARD_KIND" != "none" ]]; then
    if [[ -z "$SHARD_INDEX_RAW" || -z "$SHARD_TOTAL_RAW" ]]; then
        echo "--shard-kind=$SHARD_KIND requires --shard-index and --shard-total." >&2
        exit 1
    fi
fi

if [[ -n "$SHARD_INDEX_RAW" ]]; then
    if ! [[ "$SHARD_INDEX_RAW" =~ ^[0-9]+$ ]] || ! [[ "$SHARD_TOTAL_RAW" =~ ^[0-9]+$ ]]; then
        echo "Shard values must be non-negative integers: index='$SHARD_INDEX_RAW' total='$SHARD_TOTAL_RAW'" >&2
        exit 1
    fi
    SHARD_INDEX="$SHARD_INDEX_RAW"
    SHARD_TOTAL="$SHARD_TOTAL_RAW"
    if (( SHARD_TOTAL <= 0 )); then
        echo "--shard-total must be > 0 (got $SHARD_TOTAL)" >&2
        exit 1
    fi
    if (( SHARD_INDEX >= SHARD_TOTAL )); then
        echo "--shard-index must be < --shard-total ($SHARD_INDEX >= $SHARD_TOTAL)" >&2
        exit 1
    fi
    SHARD_INDEX_JSON="$SHARD_INDEX"
    SHARD_TOTAL_JSON="$SHARD_TOTAL"
    if [[ -z "$SHARD_NAME" ]]; then
        SHARD_NAME="${SHARD_KIND}-${SHARD_INDEX}-of-${SHARD_TOTAL}"
    fi
else
    SHARD_KIND="none"
    SHARD_NAME="${SHARD_NAME:-unsharded}"
fi

if [[ -z "$CORRELATION_ID" ]]; then
    CORRELATION_ID="${TIMESTAMP}-${SHARD_NAME}"
fi
export CI_CORRELATION_ID="$CORRELATION_ID"

if (( ${#SELECTED_UNIT_TARGETS[@]} > 0 )); then
    mapfile -t SELECTED_UNIT_TARGETS < <(printf '%s\n' "${SELECTED_UNIT_TARGETS[@]}" | awk 'NF' | LC_ALL=C sort -u)
fi
if (( ${#SELECTED_SUITES[@]} > 0 )); then
    mapfile -t SELECTED_SUITES < <(printf '%s\n' "${SELECTED_SUITES[@]}" | awk 'NF' | LC_ALL=C sort -u)
fi

select_shard_items() {
    local shard_index="$1"
    local shard_total="$2"
    shift 2
    local items=("$@")
    local i=0
    for item in "${items[@]}"; do
        if (( shard_total <= 1 || (i % shard_total) == shard_index )); then
            printf '%s\n' "$item"
        fi
        i=$((i + 1))
    done
}

if [[ "$SHARD_KIND" == "unit" || "$SHARD_KIND" == "both" ]]; then
    if (( ${#SELECTED_UNIT_TARGETS[@]} > 0 )); then
        mapfile -t SELECTED_UNIT_TARGETS < <(
            select_shard_items "$SHARD_INDEX" "$SHARD_TOTAL" "${SELECTED_UNIT_TARGETS[@]}"
        )
    fi
fi

if [[ "$SHARD_KIND" == "suite" || "$SHARD_KIND" == "both" ]]; then
    if (( ${#SELECTED_SUITES[@]} > 0 )); then
        mapfile -t SELECTED_SUITES < <(
            select_shard_items "$SHARD_INDEX" "$SHARD_TOTAL" "${SELECTED_SUITES[@]}"
        )
    fi
fi

RERUN_JSON_VALUE="null"
if [[ -n "$RERUN_FROM" ]]; then
    RERUN_JSON_VALUE="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$RERUN_FROM")"
fi

DIFF_JSON_VALUE="null"
if [[ -n "$DIFF_FROM" ]]; then
    DIFF_JSON_VALUE="$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$DIFF_FROM")"
fi

# ─── Clean Source Snapshot ───────────────────────────────────────────────────

capture_repository_snapshot() {
    python3 - "$PROJECT_ROOT" "$ARTIFACT_DIR" "${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}" <<'PY'
import hashlib
import os
import re
import stat
import subprocess
import sys
from pathlib import Path

raw_root = Path(sys.argv[1])
raw_artifact_dir = Path(sys.argv[2])
raw_cargo_target_dir = Path(sys.argv[3])


def fail(detail: str) -> None:
    print(detail, file=sys.stderr)
    raise SystemExit(1)


try:
    root_metadata = raw_root.lstat()
except OSError as exc:
    fail(f"cannot inspect repository root: {exc}")
if stat.S_ISLNK(root_metadata.st_mode) or not stat.S_ISDIR(root_metadata.st_mode):
    fail("repository root must be a real directory, not a symlink")

try:
    root = raw_root.resolve(strict=True)
    git_marker = root / ".git"
    marker_metadata = git_marker.lstat()
    if stat.S_ISLNK(marker_metadata.st_mode):
        fail("repository .git marker must not be a symlink")
    if stat.S_ISDIR(marker_metadata.st_mode):
        git_dir = git_marker.resolve(strict=True)
    elif stat.S_ISREG(marker_metadata.st_mode):
        marker = git_marker.read_text(encoding="utf-8").rstrip("\r\n")
        if "\n" in marker or "\r" in marker or not marker.startswith("gitdir: "):
            fail("repository .git file is malformed")
        target = Path(marker.removeprefix("gitdir: "))
        candidate = target if target.is_absolute() else root / target
        target_metadata = candidate.lstat()
        if stat.S_ISLNK(target_metadata.st_mode) or not stat.S_ISDIR(target_metadata.st_mode):
            fail("repository gitfile target must be a non-symlink directory")
        git_dir = candidate.resolve(strict=True)
    else:
        fail("repository .git marker is neither a directory nor a gitfile")
except (OSError, RuntimeError, UnicodeError) as exc:
    fail(f"repository Git context could not be resolved safely: {exc}")


def git(*args: str) -> bytes:
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
            "--git-dir",
            str(git_dir),
            "--work-tree",
            str(root),
            "-c",
            "core.bare=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            f"core.worktree={root}",
            *args,
        ],
        capture_output=True,
        check=False,
        env=env,
    )
    if result.returncode != 0:
        diagnostic = result.stderr.decode("utf-8", "replace").strip()
        fail(f"git {' '.join(args)} failed: {diagnostic}")
    return result.stdout


def one_canonical_path(raw: bytes, label: str) -> Path:
    try:
        text = raw.decode("utf-8", "strict")
    except UnicodeError as exc:
        fail(f"Git {label} output is not UTF-8: {exc}")
    if not text.endswith("\n") or "\n" in text.removesuffix("\n"):
        fail(f"Git {label} output is not one canonical line")
    try:
        return Path(text.removesuffix("\n")).resolve(strict=True)
    except (OSError, RuntimeError) as exc:
        fail(f"Git {label} path cannot be canonicalized: {exc}")


def split_record(record: bytes, label: str) -> tuple[bytes, bytes]:
    try:
        metadata, path = record.split(b"\t", 1)
    except ValueError:
        fail(f"malformed {label} record")
    if not path:
        fail(f"empty path in {label} record")
    return metadata, path


if one_canonical_path(git("rev-parse", "--show-toplevel"), "top-level") != root:
    fail("Git worktree does not match the filesystem-derived repository root")
if one_canonical_path(git("rev-parse", "--absolute-git-dir"), "directory") != git_dir:
    fail("Git directory does not match the filesystem-derived repository binding")

try:
    head = git("rev-parse", "--verify", "HEAD^{commit}").decode("ascii", "strict").strip()
except UnicodeError as exc:
    fail(f"HEAD object ID is not ASCII: {exc}")
if (
    len(head) not in (40, 64)
    or head.lower() != head
    or any(character not in "0123456789abcdef" for character in head)
):
    fail(f"HEAD is not a canonical full object ID: {head!r}")

tree_bytes = git("ls-tree", "-r", "-z", "--full-tree", head)
tree: dict[bytes, tuple[bytes, bytes]] = {}
for record in filter(None, tree_bytes.split(b"\0")):
    metadata, path = split_record(record, "HEAD tree")
    fields = metadata.split(b" ")
    if len(fields) != 3:
        fail("malformed HEAD tree metadata")
    mode, object_type, object_id = fields
    if mode not in (b"100644", b"100755", b"120000") or object_type != b"blob":
        fail(
            f"unsupported tracked entry at {os.fsdecode(path)!r}: "
            f"mode={mode!r} type={object_type!r}"
        )
    if path in tree:
        fail(f"duplicate path in HEAD tree: {os.fsdecode(path)!r}")
    tree[path] = (mode, object_id)

index_bytes = git("ls-files", "--stage", "-z")
index: dict[bytes, tuple[bytes, bytes]] = {}
for record in filter(None, index_bytes.split(b"\0")):
    metadata, path = split_record(record, "index")
    fields = metadata.split(b" ")
    if len(fields) != 3:
        fail("malformed index metadata")
    mode, object_id, stage = fields
    if stage != b"0":
        fail(f"non-stage-zero index entry at {os.fsdecode(path)!r}")
    if path in index:
        fail(f"duplicate path in index: {os.fsdecode(path)!r}")
    index[path] = (mode, object_id)
if index != tree:
    fail("index entries do not match the HEAD tree exactly")

flag_bytes = git("ls-files", "-v", "-z")
flag_paths: set[bytes] = set()
for record in filter(None, flag_bytes.split(b"\0")):
    if len(record) < 3 or record[1:2] != b" ":
        fail("malformed index-flag record")
    tag, path = record[:1], record[2:]
    if tag != b"H":
        fail(
            f"non-canonical index flag {tag.decode('ascii', 'replace')!r} at "
            f"{os.fsdecode(path)!r}; assume-unchanged and skip-worktree are forbidden"
        )
    if path in flag_paths:
        fail(f"duplicate path in index-flag listing: {os.fsdecode(path)!r}")
    flag_paths.add(path)
if flag_paths != set(tree):
    fail("index-flag path set does not match the HEAD tree")

untracked_bytes = git("ls-files", "--others", "--exclude-standard", "-z")
if untracked_bytes:
    untracked_paths = [
        os.fsdecode(path) for path in untracked_bytes.split(b"\0") if path
    ]
    fail("untracked non-ignored paths are present: " + ", ".join(untracked_paths[:10]))

known_generated_output_prefixes = (
    b"tests/ext_conformance/artifacts/",
    b"tests/ext_conformance/reports/",
    b"tests/artifacts/perf/",
    b"tests/evidence_bundle/",
    b".rch-tmp/",
    b".rch-target-",
)


def repository_relative_output_path(raw_path: Path) -> Path | None:
    candidate = raw_path if raw_path.is_absolute() else root / raw_path
    try:
        return Path(os.path.abspath(candidate)).relative_to(root)
    except (OSError, ValueError):
        return None


allowed_output_prefixes = set(known_generated_output_prefixes)
artifact_relative = repository_relative_output_path(raw_artifact_dir)
if artifact_relative is not None:
    artifact_parts = artifact_relative.parts
    if (
        len(artifact_parts) != 3
        or artifact_parts[:2] != ("tests", "e2e_results")
        or re.fullmatch(r"[0-9]{8}T[0-9]{6}Z", artifact_parts[2]) is None
    ):
        fail(
            "in-repository E2E artifact directory must be one canonical timestamp child "
            "of tests/e2e_results"
        )
    allowed_output_prefixes.add(os.fsencode(artifact_relative) + b"/")

cargo_target_relative = repository_relative_output_path(raw_cargo_target_dir)
if cargo_target_relative is not None:
    cargo_target_parts = cargo_target_relative.parts
    target_root = cargo_target_parts[0] if cargo_target_parts else ""
    if re.fullmatch(
        r"(?:target(?:[-_].+)?|\.cargo-target.*|\.target[-_].+|_target[-_].+|"
        r"\.rch-target-.+|bd-.+)",
        target_root,
    ) is None:
        fail(
            "in-repository CARGO_TARGET_DIR must be under an approved root-level "
            "Cargo target/cache directory"
        )
    allowed_output_prefixes.add(os.fsencode(cargo_target_relative) + b"/")


def ignored_path_is_generated_output(path: bytes) -> bool:
    return any(path.startswith(prefix) for prefix in allowed_output_prefixes)


def ignored_source_bytes() -> bytes:
    ignored_bytes = git("ls-files", "--others", "--ignored", "--exclude-standard", "-z")
    paths = [
        path
        for path in ignored_bytes.split(b"\0")
        if path and not ignored_path_is_generated_output(path)
    ]
    if paths:
        decoded = [os.fsdecode(path) for path in paths]
        fail(
            "ignored untracked paths outside approved generated-output roots are present: "
            + ", ".join(decoded[:10])
        )
    return b""


initial_ignored_source_bytes = ignored_source_bytes()

root_bytes = os.fsencode(root)


def framed_field(value: bytes) -> bytes:
    return str(len(value)).encode("ascii") + b":" + value


def capture_raw_source_digest() -> str:
    digest = hashlib.sha256()
    digest.update(framed_field(b"pi.e2e.source_snapshot.v1"))
    digest.update(framed_field(head.encode("ascii")))
    digest.update(framed_field(tree_bytes))
    digest.update(framed_field(index_bytes))
    digest.update(framed_field(flag_bytes))

    for path, (expected_mode, expected_object_id) in tree.items():
        full_path = os.path.join(root_bytes, path)
        parent = os.path.dirname(full_path)
        while parent != root_bytes:
            try:
                parent_metadata = os.lstat(parent)
            except OSError as exc:
                fail(f"cannot inspect parent of {os.fsdecode(path)!r}: {exc}")
            if stat.S_ISLNK(parent_metadata.st_mode):
                fail(f"tracked path traverses a symlinked parent: {os.fsdecode(path)!r}")
            if not stat.S_ISDIR(parent_metadata.st_mode):
                fail(f"tracked path traverses a non-directory parent: {os.fsdecode(path)!r}")
            next_parent = os.path.dirname(parent)
            if next_parent == parent or not parent.startswith(root_bytes + os.sep.encode()):
                fail(f"tracked path escapes repository root: {os.fsdecode(path)!r}")
            parent = next_parent

        try:
            file_metadata = os.lstat(full_path)
        except OSError as exc:
            fail(f"cannot inspect tracked path {os.fsdecode(path)!r}: {exc}")

        if expected_mode in (b"100644", b"100755"):
            if not stat.S_ISREG(file_metadata.st_mode):
                fail(f"tracked regular file has wrong worktree type: {os.fsdecode(path)!r}")
            actual_mode = b"100755" if file_metadata.st_mode & 0o111 else b"100644"
            if actual_mode != expected_mode:
                fail(
                    f"raw worktree mode differs from HEAD at {os.fsdecode(path)!r}: "
                    f"expected={expected_mode.decode('ascii')} actual={actual_mode.decode('ascii')}"
                )
            try:
                with open(full_path, "rb") as handle:
                    contents = handle.read()
            except OSError as exc:
                fail(f"cannot read tracked path {os.fsdecode(path)!r}: {exc}")
        else:
            if not stat.S_ISLNK(file_metadata.st_mode):
                fail(f"tracked symlink has wrong worktree type: {os.fsdecode(path)!r}")
            actual_mode = b"120000"
            try:
                contents = os.readlink(full_path)
            except OSError as exc:
                fail(f"cannot read tracked symlink {os.fsdecode(path)!r}: {exc}")

        framed_blob = b"blob " + str(len(contents)).encode("ascii") + b"\0" + contents
        if len(expected_object_id) == 40:
            actual_object_id = hashlib.sha1(framed_blob).hexdigest().encode("ascii")
        elif len(expected_object_id) == 64:
            actual_object_id = hashlib.sha256(framed_blob).hexdigest().encode("ascii")
        else:
            fail(f"unsupported Git object ID length for {os.fsdecode(path)!r}")
        if actual_object_id != expected_object_id:
            fail(f"raw worktree bytes differ from HEAD at {os.fsdecode(path)!r}")

        digest.update(framed_field(path))
        digest.update(framed_field(actual_mode))
        digest.update(framed_field(contents))

    return digest.hexdigest()


def verify_git_metadata_unchanged() -> None:
    try:
        current_head = git("rev-parse", "--verify", "HEAD^{commit}").decode(
            "ascii", "strict"
        ).strip()
    except UnicodeError as exc:
        fail(f"HEAD object ID became invalid while capturing source: {exc}")
    if current_head != head:
        fail("HEAD changed while repository state was captured")
    if git("ls-tree", "-r", "-z", "--full-tree", head) != tree_bytes:
        fail("HEAD tree changed while repository state was captured")
    if git("ls-files", "--stage", "-z") != index_bytes:
        fail("index changed while repository state was captured")
    if git("ls-files", "-v", "-z") != flag_bytes:
        fail("index flags changed while repository state was captured")
    if git("ls-files", "--others", "--exclude-standard", "-z") != untracked_bytes:
        fail("untracked path set changed while repository state was captured")
    if ignored_source_bytes() != initial_ignored_source_bytes:
        fail("ignored source path set changed while repository state was captured")


initial_digest = capture_raw_source_digest()
verify_git_metadata_unchanged()
final_digest = capture_raw_source_digest()
if final_digest != initial_digest:
    fail("raw tracked worktree bytes or modes changed while repository state was captured")
verify_git_metadata_unchanged()

print(f"{head}|sha256:{final_digest}")
PY
}

initialize_source_snapshot() {
    local captured_snapshot extra_field
    if ! captured_snapshot="$(capture_repository_snapshot 2>&1)"; then
        echo "[source] FAIL: repository is not byte-for-byte clean at snapshot time: $captured_snapshot" >&2
        return 1
    fi
    IFS='|' read -r SOURCE_COMMIT SOURCE_SNAPSHOT extra_field <<<"$captured_snapshot"
    if [[ -n "$extra_field" ]] || \
        [[ ! "$SOURCE_COMMIT" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || \
        [[ ! "$SOURCE_SNAPSHOT" =~ ^sha256:[0-9a-f]{64}$ ]]; then
        echo "[source] FAIL: repository snapshot helper returned malformed identity" >&2
        SOURCE_COMMIT=""
        SOURCE_SNAPSHOT=""
        return 1
    fi
    echo "[source] Captured clean source snapshot at $SOURCE_COMMIT ($SOURCE_SNAPSHOT)"
}

verify_source_snapshot_unchanged() {
    local final_capture final_commit final_snapshot extra_field
    if ! final_capture="$(capture_repository_snapshot 2>&1)"; then
        echo "[source] FAIL: final repository snapshot could not be captured: $final_capture" >&2
        return 1
    fi
    IFS='|' read -r final_commit final_snapshot extra_field <<<"$final_capture"
    if [[ -n "$extra_field" ]] || \
        [[ "$final_commit" != "$SOURCE_COMMIT" ]] || \
        [[ "$final_snapshot" != "$SOURCE_SNAPSHOT" ]]; then
        echo "[source] FAIL: repository source changed during E2E verification" >&2
        echo "[source] Initial: $SOURCE_COMMIT|$SOURCE_SNAPSHOT" >&2
        echo "[source] Final:   $final_capture" >&2
        return 1
    fi
    echo "[source] PASS: final source snapshot matches $SOURCE_SNAPSHOT"
}

capture_diagnostic_artifacts_json() {
    local output_log_path="$1"
    local test_log_path="${2:-}"
    local artifact_index_path="${3:-}"

    python3 - "$output_log_path" "$test_log_path" "$artifact_index_path" <<'PY'
import hashlib
import json
import os
import stat
import sys
from pathlib import Path


def fail(detail: str) -> None:
    print(f"diagnostic artifact binding failed: {detail}", file=sys.stderr)
    raise SystemExit(1)


def capture_file(raw_path: str, label: str) -> dict | None:
    if not raw_path:
        return None
    path = Path(os.path.abspath(raw_path))
    parent = path.parent
    while True:
        try:
            parent_metadata = parent.lstat()
        except OSError as exc:
            fail(f"cannot inspect parent of {label}: {parent}: {exc}")
        if stat.S_ISLNK(parent_metadata.st_mode) or not stat.S_ISDIR(parent_metadata.st_mode):
            fail(f"{label} traverses a symlinked or non-directory parent: {parent}")
        if parent == parent.parent:
            break
        parent = parent.parent
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as exc:
        fail(f"cannot open {label} as a non-symlink regular file: {path}: {exc}")

    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            fail(f"{label} is not a regular file: {path}")
        if before.st_mode & 0o111:
            fail(f"{label} must not be executable: {path}")

        digest = hashlib.sha256()
        size = 0
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
            size += len(chunk)
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)

    stable_fields = ("st_dev", "st_ino", "st_mode", "st_size", "st_mtime_ns", "st_ctime_ns")
    if any(getattr(before, field) != getattr(after, field) for field in stable_fields):
        fail(f"{label} changed while its raw bytes were hashed: {path}")
    if size != after.st_size:
        fail(f"{label} byte count changed while it was hashed: {path}")
    try:
        final_metadata = path.lstat()
    except OSError as exc:
        fail(f"cannot re-inspect {label}: {path}: {exc}")
    if any(getattr(after, field) != getattr(final_metadata, field) for field in stable_fields):
        fail(f"{label} path was replaced while its raw bytes were hashed: {path}")

    return {
        "path": raw_path,
        "sha256": f"sha256:{digest.hexdigest()}",
        "size_bytes": size,
    }


output_log, test_log, artifact_index = sys.argv[1:]
payload = {
    "schema": "pi.e2e.diagnostic_artifacts.v1",
    "output_log": capture_file(output_log, "output log"),
    "test_log_jsonl": capture_file(test_log, "test log JSONL"),
    "artifact_index_jsonl": capture_file(artifact_index, "artifact index JSONL"),
}
print(json.dumps(payload, separators=(",", ":"), sort_keys=True))
PY
}

# ─── Environment Capture ─────────────────────────────────────────────────────

prepare_artifact_directory() {
    if [[ -L "$ARTIFACT_DIR" ]]; then
        echo "[preflight] E2E artifact directory must not be a symlink: $ARTIFACT_DIR" >&2
        return 1
    fi
    if [[ -e "$ARTIFACT_DIR" && ! -d "$ARTIFACT_DIR" ]]; then
        echo "[preflight] E2E artifact path is not a directory: $ARTIFACT_DIR" >&2
        return 1
    fi
    if ! mkdir -p "$ARTIFACT_DIR"; then
        echo "[preflight] Unable to create E2E artifact directory: $ARTIFACT_DIR" >&2
        return 1
    fi

    local first_entry
    if ! first_entry="$(find "$ARTIFACT_DIR" -mindepth 1 -maxdepth 1 -print -quit)"; then
        echo "[preflight] Unable to inspect E2E artifact directory: $ARTIFACT_DIR" >&2
        return 1
    fi
    if [[ -n "$first_entry" ]]; then
        echo "[preflight] E2E artifact directory must be empty; refusing to reuse prior evidence: $ARTIFACT_DIR" >&2
        return 1
    fi
}

check_disk_headroom() {
    local min_free_mb="${VERIFY_MIN_FREE_MB:-2048}"
    local min_tmp_free_mb="${VERIFY_MIN_TMP_FREE_MB:-8192}"
    local min_inode_free_pct="${VERIFY_MIN_FREE_INODE_PCT:-5}"
    local tmp_dir="${TMPDIR:-/tmp}"
    local cargo_target_dir="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"
    local cargo_home="${CARGO_HOME:-${HOME:-}/.cargo}"

    if ! [[ "$min_free_mb" =~ ^[0-9]+$ ]] || [[ "$min_free_mb" -le 0 ]]; then
        echo "[preflight] Invalid VERIFY_MIN_FREE_MB='$min_free_mb' (must be positive integer)" >&2
        return 1
    fi
    if ! [[ "$min_tmp_free_mb" =~ ^[0-9]+$ ]] || [[ "$min_tmp_free_mb" -le 0 ]]; then
        echo "[preflight] Invalid VERIFY_MIN_TMP_FREE_MB='$min_tmp_free_mb' (must be positive integer)" >&2
        return 1
    fi
    if ! [[ "$min_inode_free_pct" =~ ^[0-9]+$ ]] || [[ "$min_inode_free_pct" -le 0 || "$min_inode_free_pct" -ge 100 ]]; then
        echo "[preflight] Invalid VERIFY_MIN_FREE_INODE_PCT='$min_inode_free_pct' (must be integer 1-99)" >&2
        return 1
    fi

    local min_free_kb=$((min_free_mb * 1024))
    local min_tmp_free_kb=$((min_tmp_free_mb * 1024))
    local failed=false
    local -a probe_specs=(
        "$PROJECT_ROOT|repo|$min_free_kb|$min_free_mb"
        "$ARTIFACT_DIR|artifacts|$min_free_kb|$min_free_mb"
        "$tmp_dir|tmp|$min_tmp_free_kb|$min_tmp_free_mb"
        "$cargo_target_dir|cargo_target|$min_tmp_free_kb|$min_tmp_free_mb"
        "$cargo_home|cargo_home|$min_tmp_free_kb|$min_tmp_free_mb"
    )

    echo "[preflight] Disk headroom check: repo/artifacts >=${min_free_mb}MB, tmp/cargo >=${min_tmp_free_mb}MB, inodes >=${min_inode_free_pct}%"

    for probe_spec in "${probe_specs[@]}"; do
        IFS='|' read -r raw_path probe_label required_kb required_mb <<<"$probe_spec"

        local probe_path="$raw_path"
        while [[ ! -e "$probe_path" && "$probe_path" != "/" ]]; do
            probe_path="$(dirname "$probe_path")"
        done

        if [[ ! -e "$probe_path" ]]; then
            echo "[preflight] WARN: cannot probe path '$raw_path' (no existing ancestor found)" >&2
            continue
        fi

        local disk_row
        disk_row="$(df -Pk "$probe_path" | awk 'NR==2 {print $4 "|" $6}')"
        if [[ -z "$disk_row" ]]; then
            echo "[preflight] WARN: unable to read disk stats for '$probe_path'" >&2
            failed=true
            continue
        fi

        local avail_kb mount_point
        avail_kb="${disk_row%%|*}"
        mount_point="${disk_row#*|}"

        local inode_used_pct inode_free_pct inode_free_display
        local inode_check_available=true
        inode_used_pct="$(df -Pi "$probe_path" | awk 'NR==2 {gsub(/%/, "", $5); print $5}')"
        if [[ "$inode_used_pct" =~ ^[0-9]+$ ]]; then
            inode_free_pct=$((100 - inode_used_pct))
            inode_free_display="${inode_free_pct}%"
        else
            inode_free_pct="unknown"
            inode_free_display="unknown"
            inode_check_available=false
            echo "[preflight] WARN: inode utilization is unavailable for '$probe_path'" >&2
        fi

        local avail_mb
        avail_mb=$((avail_kb / 1024))
        echo "[preflight] target=$probe_label mount=$mount_point free=${avail_mb}MB inode_free=$inode_free_display path=$raw_path"

        if (( avail_kb < required_kb )); then
            echo "[preflight] FAIL: target '$probe_label' on mount '$mount_point' has ${avail_mb}MB free (< ${required_mb}MB required)" >&2
            failed=true
        fi
        if $inode_check_available && (( inode_free_pct < min_inode_free_pct )); then
            echo "[preflight] FAIL: mount '$mount_point' has ${inode_free_pct}% free inodes (< ${min_inode_free_pct}% required)" >&2
            failed=true
        fi
    done

    if $failed; then
        cat >&2 <<EOF
[preflight] Verification aborted: insufficient filesystem headroom.
[preflight] Free space on the workspace filesystem is too low for cargo build/test artifacts.
[preflight] Fix by freeing disk space, or moving temp/build/artifact paths to roomier mounts:
[preflight]   TMPDIR=/data/tmp CARGO_TARGET_DIR=/data/tmp/pi_verify_target ./verify --profile quick
[preflight]   CARGO_HOME=/data/tmp/pi_cargo ./verify --profile quick
[preflight]   E2E_ARTIFACT_DIR=/data/tmp/pi_e2e_results/$TIMESTAMP ./verify --profile quick
[preflight] You can also tune thresholds:
[preflight]   VERIFY_MIN_FREE_MB=1024 VERIFY_MIN_TMP_FREE_MB=2048 ./verify --profile quick
EOF
        return 1
    fi

    echo "[preflight] PASS"
    return 0
}
capture_env() {
    local env_file="$ARTIFACT_DIR/environment.json"
    local rustc_version cargo_version os_info git_sha git_branch
    mkdir -p "$ARTIFACT_DIR"
    rustc_version="$(rustc --version 2>/dev/null || echo 'unknown')"
    cargo_version="$(run_cargo --version 2>/dev/null || echo 'unknown')"
    os_info="$(uname -srm 2>/dev/null || echo 'unknown')"
    git_sha="$SOURCE_COMMIT"
    git_branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo 'unknown')"

    cat > "$env_file" <<ENVJSON
{
  "schema": "pi.e2e.environment.v1",
  "generated_at": "$GENERATED_AT",
  "timestamp": "$TIMESTAMP",
  "profile": "$PROFILE",
  "rerun_from": $RERUN_JSON_VALUE,
  "diff_from": $DIFF_JSON_VALUE,
  "rustc": "$rustc_version",
  "cargo": "$cargo_version",
  "cargo_runner": "$CARGO_RUNNER_DESC",
  "cargo_exec_prefix": $CARGO_EXEC_PREFIX_JSON,
  "os": "$os_info",
  "git_sha": "$git_sha",
  "git_branch": "$git_branch",
  "source_commit": "$SOURCE_COMMIT",
  "source_snapshot": "$SOURCE_SNAPSHOT",
  "parallelism": $PARALLELISM,
  "log_level": "$LOG_LEVEL",
  "artifact_dir": "$ARTIFACT_DIR",
  "correlation_id": "$CORRELATION_ID",
  "shard": {
    "kind": "$SHARD_KIND",
    "name": "$SHARD_NAME",
    "index": $SHARD_INDEX_JSON,
    "total": $SHARD_TOTAL_JSON
  },
  "cargo_target_dir": "${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}",
  "vcr_mode": "${VCR_MODE:-unset}",
  "unit_targets": $(printf '%s\n' "${SELECTED_UNIT_TARGETS[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'),
  "e2e_suites": $(printf '%s\n' "${SELECTED_SUITES[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]')
}
ENVJSON
    echo "[env] Captured environment to $env_file"
}

write_shard_manifest() {
    local manifest_file="$ARTIFACT_DIR/ci_shard_manifest.json"
    cat > "$manifest_file" <<SHARDJSON
{
  "schema": "pi.verify.shard_manifest.v1",
  "generated_at": "$TIMESTAMP",
  "profile": "$PROFILE",
  "artifact_dir": "$ARTIFACT_DIR",
  "correlation_id": "$CORRELATION_ID",
  "cargo_runner": "$CARGO_RUNNER_DESC",
  "cargo_exec_prefix": $CARGO_EXEC_PREFIX_JSON,
  "shard": {
    "kind": "$SHARD_KIND",
    "name": "$SHARD_NAME",
    "index": $SHARD_INDEX_JSON,
    "total": $SHARD_TOTAL_JSON
  },
  "selection": {
    "unit_targets": $(printf '%s\n' "${SELECTED_UNIT_TARGETS[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'),
    "e2e_suites": $(printf '%s\n' "${SELECTED_SUITES[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]')
  }
}
SHARDJSON
    echo "[shard] Manifest written to $manifest_file"
}

# ─── Lint Gates ──────────────────────────────────────────────────────────────

run_lint_gates() {
    if $SKIP_LINT; then
        echo "[lint] Skipped (--skip-lint)"
        return 0
    fi

    local lint_dir="$ARTIFACT_DIR/lint"
    mkdir -p "$lint_dir"
    local lint_ok=true

    echo "[lint] Running format check..."
    if run_cargo fmt --check > "$lint_dir/fmt.log" 2>&1; then
        echo "[lint] cargo fmt: PASS"
    else
        echo "[lint] cargo fmt: FAIL (see $lint_dir/fmt.log)"
        lint_ok=false
    fi

    echo "[lint] Running clippy..."
    if run_split_clippy_gates "$lint_dir/clippy.log"; then
        echo "[lint] clippy: PASS"
    else
        echo "[lint] clippy: FAIL (see $lint_dir/clippy.log)"
        lint_ok=false
    fi

    if $lint_ok; then
        echo "[lint] All gates passed"
        return 0
    else
        echo "[lint] Some gates failed" >&2
        return 1
    fi
}

run_split_clippy_gates() {
    local log_file="$1"
    : > "$log_file"

    local -a labels=("lib+bins" "tests" "benches" "examples")
    local -a args=()
    for label in "${labels[@]}"; do
        echo "=== clippy slice: $label ===" >> "$log_file"
        case "$label" in
            "lib+bins")
                args=(clippy --locked --lib --bins -- -D warnings)
                ;;
            "tests")
                args=(clippy --locked --tests -- -D warnings)
                ;;
            "benches")
                args=(clippy --locked --benches -- -D warnings)
                ;;
            "examples")
                args=(clippy --locked --examples -- -D warnings)
                ;;
        esac

        if run_cargo "${args[@]}" >> "$log_file" 2>&1; then
            echo "slice $label: PASS" >> "$log_file"
            echo >> "$log_file"
        else
            echo "slice $label: FAIL" >> "$log_file"
            echo >> "$log_file"
            return 1
        fi
    done

    return 0
}

# ─── Lib Inline Tests ────────────────────────────────────────────────────────

run_lib_tests() {
    local lib_dir="$ARTIFACT_DIR/lib"
    local log_file="$lib_dir/output.log"
    local result_file="$lib_dir/result.json"
    mkdir -p "$lib_dir"

    echo "[lib] Running inline unit tests (cargo test --lib)..."
    local start_epoch exit_code duration_ms
    start_epoch=$(date +%s%N 2>/dev/null || date +%s)

    set +e
    run_cargo test --locked --lib -- --test-threads="$PARALLELISM" 2>&1 | tee "$log_file"
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

    redact_secrets
    local diagnostic_artifacts_json
    diagnostic_artifacts_json="$(capture_diagnostic_artifacts_json "$log_file")"

    cat > "$result_file" <<RESULTJSON
{
  "schema": "pi.e2e.result.v1",
  "result_kind": "lib",
  "correlation_id": "$CORRELATION_ID",
  "target": "lib",
  "exit_code": $exit_code,
  "duration_ms": $duration_ms,
  "passed": $passed,
  "failed": $failed,
  "ignored": $ignored,
  "total": $total,
  "log_file": "$log_file",
  "diagnostic_artifacts": $diagnostic_artifacts_json,
  "timestamp": "$TIMESTAMP"
}
RESULTJSON

    if [[ $exit_code -eq 0 ]]; then
        echo "[lib] PASS ($passed passed, $ignored ignored, ${duration_ms}ms)"
    else
        echo "[lib] FAIL (exit $exit_code, $failed failed, $passed passed, ${duration_ms}ms)"
    fi

    return "$exit_code"
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
        if ! run_cargo test --locked --test "$target" --no-run 2>>"$build_log"; then
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
        if ! run_cargo test --locked --test "$suite" --no-run 2>>"$build_log"; then
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

    local test_log_env_path artifact_index_env_path
    test_log_env_path="$(path_for_test_harness "$target_dir/test-log.jsonl")"
    artifact_index_env_path="$(path_for_test_harness "$target_dir/artifact-index.jsonl")"
    : > "$target_dir/test-log.jsonl"
    : > "$target_dir/artifact-index.jsonl"
    export TEST_LOG_JSONL_PATH="$test_log_env_path"
    export TEST_ARTIFACT_INDEX_PATH="$artifact_index_env_path"
    export RUST_LOG="$LOG_LEVEL"

    set +e
    run_cargo test \
        --locked \
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

    redact_secrets
    local diagnostic_artifacts_json
    diagnostic_artifacts_json="$(
        capture_diagnostic_artifacts_json \
            "$log_file" \
            "$target_dir/test-log.jsonl" \
            "$target_dir/artifact-index.jsonl"
    )"

    cat > "$result_file" <<RESULTJSON
{
  "schema": "pi.e2e.result.v1",
  "result_kind": "unit",
  "correlation_id": "$CORRELATION_ID",
  "target": "$target",
  "exit_code": $exit_code,
  "duration_ms": $duration_ms,
  "passed": $passed,
  "failed": $failed,
  "ignored": $ignored,
  "total": $total,
  "log_file": "$log_file",
  "test_log_jsonl": "$target_dir/test-log.jsonl",
  "artifact_index_jsonl": "$target_dir/artifact-index.jsonl",
  "diagnostic_artifacts": $diagnostic_artifacts_json,
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

    return "$exit_code"
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
    local test_log_env_path artifact_index_env_path
    test_log_env_path="$(path_for_test_harness "$suite_dir/test-log.jsonl")"
    artifact_index_env_path="$(path_for_test_harness "$suite_dir/artifact-index.jsonl")"
    : > "$suite_dir/test-log.jsonl"
    : > "$suite_dir/artifact-index.jsonl"
    export TEST_LOG_JSONL_PATH="$test_log_env_path"
    export TEST_ARTIFACT_INDEX_PATH="$artifact_index_env_path"
    export RUST_LOG="$LOG_LEVEL"

    set +e
    run_cargo test \
        --locked \
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

    redact_secrets
    local diagnostic_artifacts_json
    diagnostic_artifacts_json="$(
        capture_diagnostic_artifacts_json \
            "$log_file" \
            "$suite_dir/test-log.jsonl" \
            "$suite_dir/artifact-index.jsonl"
    )"

    cat > "$result_file" <<RESULTJSON
{
  "schema": "pi.e2e.result.v1",
  "result_kind": "suite",
  "correlation_id": "$CORRELATION_ID",
  "suite": "$suite",
  "exit_code": $exit_code,
  "duration_ms": $duration_ms,
  "passed": $passed,
  "failed": $failed,
  "ignored": $ignored,
  "total": $total,
  "log_file": "$log_file",
  "test_log_jsonl": "$suite_dir/test-log.jsonl",
  "artifact_index_jsonl": "$suite_dir/artifact-index.jsonl",
  "diagnostic_artifacts": $diagnostic_artifacts_json,
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

    return "$exit_code"
}

# ─── Summary Manifest ────────────────────────────────────────────────────────

write_summary() {
    local summary_file="$ARTIFACT_DIR/summary.json"
    local lib_result_json="null"
    local total_units=${#SELECTED_UNIT_TARGETS[@]}
    local passed_units=0
    local failed_units=0
    local failed_unit_names=()
    local total_suites=${#SELECTED_SUITES[@]}
    local passed_suites=0
    local failed_suites=0
    local failed_names=()

    echo "[summary] Writing manifest to $summary_file"

    # Freeze all human-readable diagnostics before embedding their byte
    # identities from the per-target result documents into the summary.
    redact_secrets

    if [[ -f "$ARTIFACT_DIR/lib/result.json" ]]; then
        lib_result_json="$(cat "$ARTIFACT_DIR/lib/result.json")"
    fi

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

    cat > "$summary_file" <<SUMMARYJSON
{
  "schema": "pi.e2e.summary.v1",
  "generated_at": "$GENERATED_AT",
  "timestamp": "$TIMESTAMP",
  "profile": "$PROFILE",
  "rerun_from": $RERUN_JSON_VALUE,
  "diff_from": $DIFF_JSON_VALUE,
  "artifact_dir": "$ARTIFACT_DIR",
  "correlation_id": "$CORRELATION_ID",
  "source_commit": "$SOURCE_COMMIT",
  "source_snapshot": "$SOURCE_SNAPSHOT",
  "shard": {
    "kind": "$SHARD_KIND",
    "name": "$SHARD_NAME",
    "index": $SHARD_INDEX_JSON,
    "total": $SHARD_TOTAL_JSON
  },
  "total_units": $total_units,
  "passed_units": $passed_units,
  "failed_units": $failed_units,
  "failed_unit_names": $(printf '%s\n' "${failed_unit_names[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'),
  "total_suites": $total_suites,
  "passed_suites": $passed_suites,
  "failed_suites": $failed_suites,
  "failed_names": $(printf '%s\n' "${failed_names[@]:-}" | python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'),
  "lib": $lib_result_json,
  "runner_outcome": null,
  "unit_targets": $unit_results_array,
  "suites": $suite_results_array
}
SUMMARYJSON

    # Lint status.
    local lint_status="skip"
    if ! $SKIP_LINT; then
        if [[ -f "$ARTIFACT_DIR/lint/fmt.log" ]] && [[ -f "$ARTIFACT_DIR/lint/clippy.log" ]]; then
            local fmt_ok clippy_ok
            fmt_ok=$([[ $(wc -c < "$ARTIFACT_DIR/lint/fmt.log") -eq 0 ]] && echo "pass" || echo "fail")
            clippy_ok=$(grep -q "error" "$ARTIFACT_DIR/lint/clippy.log" 2>/dev/null && echo "fail" || echo "pass")
            lint_status="${fmt_ok}/${clippy_ok}"
        else
            lint_status="not_run"
        fi
    fi

    # Lib test status.
    local lib_status="not_run"
    if [[ -f "$ARTIFACT_DIR/lib/result.json" ]]; then
        local lib_exit
        lib_exit=$(python3 -c "import json; print(json.load(open('$ARTIFACT_DIR/lib/result.json'))['exit_code'])" 2>/dev/null || echo "1")
        lib_status=$( [[ "$lib_exit" -eq 0 ]] && echo "pass" || echo "fail" )
    fi

    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo " Verification Summary (profile: $PROFILE)"
    echo " Correlation:    $CORRELATION_ID"
    echo " Shard:          kind=$SHARD_KIND name=$SHARD_NAME index=${SHARD_INDEX_JSON} total=${SHARD_TOTAL_JSON}"
    echo " Lint gates:     $lint_status"
    echo " Lib inline:     $lib_status"
    echo " Integration:    $passed_units/$total_units passed"
    echo " E2E suites:     $passed_suites/$total_suites passed"
    if [[ $failed_units -gt 0 ]]; then
        echo " Failed targets: ${failed_unit_names[*]}"
    fi
    if [[ $failed_suites -gt 0 ]]; then
        echo " Failed E2E:     ${failed_names[*]}"
    fi
    echo " Artifacts: $ARTIFACT_DIR"
    echo "═══════════════════════════════════════════════════════════════"
}

write_runner_outcome() {
    local exit_code="$1"
    local source_snapshot_verified="$2"
    shift 2
    local failed_phases_json
    failed_phases_json="$(
        printf '%s\n' "$@" | \
            python3 -c 'import json,sys; print(json.dumps([line.strip() for line in sys.stdin if line.strip()]))'
    )"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        GENERATED_AT="$GENERATED_AT" \
        TIMESTAMP="$TIMESTAMP" \
        VERIFY_PROFILE_NAME="$PROFILE" \
        CORRELATION_ID="$CORRELATION_ID" \
        SOURCE_COMMIT="$SOURCE_COMMIT" \
        SOURCE_SNAPSHOT="$SOURCE_SNAPSHOT" \
        RUNNER_EXIT_CODE="$exit_code" \
        SOURCE_SNAPSHOT_VERIFIED="$source_snapshot_verified" \
        FAILED_PHASES_JSON="$failed_phases_json" \
        python3 - <<'PY'
import json
import os
import re
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
summary_path = artifact_dir / "summary.json"
outcome_path = artifact_dir / "runner_outcome.json"

try:
    exit_code = int(os.environ["RUNNER_EXIT_CODE"])
except ValueError as exc:
    raise SystemExit(f"runner exit code is invalid: {exc}") from exc
if exit_code not in (0, 1):
    raise SystemExit(f"runner exit code must be 0 or 1, got {exit_code}")

source_snapshot_verified_raw = os.environ["SOURCE_SNAPSHOT_VERIFIED"]
if source_snapshot_verified_raw not in {"true", "false"}:
    raise SystemExit("source snapshot verification flag must be true or false")
source_snapshot_verified = source_snapshot_verified_raw == "true"

try:
    failed_phases = json.loads(os.environ["FAILED_PHASES_JSON"])
except json.JSONDecodeError as exc:
    raise SystemExit(f"failed phase list is invalid JSON: {exc}") from exc
if (
    not isinstance(failed_phases, list)
    or any(
        not isinstance(phase, str)
        or re.fullmatch(r"[A-Za-z0-9_.:-]+", phase) is None
        for phase in failed_phases
    )
    or len(failed_phases) != len(set(failed_phases))
):
    raise SystemExit("failed phase list must contain unique canonical identifiers")

passed = exit_code == 0 and source_snapshot_verified and not failed_phases
if (exit_code == 0) != passed:
    raise SystemExit(
        "runner exit code 0 requires a verified source snapshot and no failed phases"
    )

outcome = {
    "schema": "pi.e2e.runner_outcome.v1",
    "generated_at": os.environ["GENERATED_AT"],
    "timestamp": os.environ["TIMESTAMP"],
    "profile": os.environ["VERIFY_PROFILE_NAME"],
    "artifact_dir": str(artifact_dir),
    "correlation_id": os.environ["CORRELATION_ID"],
    "source_commit": os.environ["SOURCE_COMMIT"],
    "source_snapshot": os.environ["SOURCE_SNAPSHOT"],
    "status": "pass" if passed else "fail",
    "exit_code": exit_code,
    "source_snapshot_verified": source_snapshot_verified,
    "failed_phases": failed_phases,
}

try:
    summary = json.loads(summary_path.read_text(encoding="utf-8"))
except (OSError, UnicodeError, json.JSONDecodeError) as exc:
    raise SystemExit(f"cannot load summary before writing runner outcome: {exc}") from exc
if not isinstance(summary, dict):
    raise SystemExit("summary must be an object before writing runner outcome")

outcome_path.write_text(json.dumps(outcome, indent=2) + "\n", encoding="utf-8")
summary["runner_outcome"] = outcome
summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
PY
    then
        echo "[outcome] Recorded runner exit=$exit_code source_verified=$source_snapshot_verified"
        return 0
    fi

    echo "[outcome] FAIL: unable to record the final runner outcome" >&2
    return 1
}

# ─── Structured Failure Diagnostics (bd-1f42.8.6.4) ─────────────────────────

generate_failure_diagnostics() {
    local diagnostics_index="$ARTIFACT_DIR/failure_diagnostics_index.json"
    local run_timeline="$ARTIFACT_DIR/failure_timeline.jsonl"
    local summary_file="$ARTIFACT_DIR/summary.json"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        SUMMARY_FILE="$summary_file" \
        DIAGNOSTICS_INDEX="$diagnostics_index" \
        RUN_TIMELINE="$run_timeline" \
        python3 - <<'PY'
import json
import os
import re
from datetime import datetime, timedelta, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
summary_file = Path(os.environ["SUMMARY_FILE"])
diagnostics_index = Path(os.environ["DIAGNOSTICS_INDEX"])
run_timeline_path = Path(os.environ["RUN_TIMELINE"])

ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
TEST_RESULT_RE = re.compile(r"^test\s+([A-Za-z0-9_:\-./]+)\s+\.\.\.\s+(ok|FAILED|ignored)$")
FAILURE_HEADER_RE = re.compile(r"^----\s+(.+?)\s+stdout\s+----$")
PANIC_RE = re.compile(r"^thread '([^']+)'(?: \([^)]*\))? panicked at (.+?):(\d+):(\d+):$")


def read_json(path: Path):
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def strip_ansi(text: str) -> str:
    return ANSI_RE.sub("", text)


def dedupe_preserve(items: list[str]) -> list[str]:
    seen: set[str] = set()
    output: list[str] = []
    for item in items:
        value = str(item).strip()
        if not value or value in seen:
            continue
        seen.add(value)
        output.append(value)
    return output


def as_int(value: object, default: int) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def classify_root_cause(message: str, raw_log: str) -> str:
    haystack = f"{message}\n{raw_log}".lower()
    if "timed out" in haystack or "timeout" in haystack:
        return "timeout"
    if (
        "no matching interaction found in cassette" in haystack
        or "vcr cassette missing or invalid" in haystack
        or "match criteria: method + url + body + body_text" in haystack
    ):
        return "vcr_mismatch"
    if "assertion" in haystack or "assert_eq!" in haystack or "assert_ne!" in haystack:
        return "assertion_failure"
    if "permission denied" in haystack:
        return "permission_denied"
    if "connection refused" in haystack or "connection reset" in haystack:
        return "network_io"
    if "clippy" in haystack and "error:" in haystack:
        return "lint_failure"
    if "not found" in haystack and ("file" in haystack or "path" in haystack):
        return "missing_file"
    if "panicked at" in haystack or "panic" in haystack:
        return "panic"
    return "unknown"


def remediation_summary(root_cause_class: str) -> str:
    if root_cause_class == "timeout":
        return "Inspect stalled operations and timeout thresholds; use timeline gaps to isolate hung steps."
    if root_cause_class == "vcr_mismatch":
        return (
            "Re-record or refresh the affected cassette, and verify dynamic prompt context "
            "(cwd/project instructions/timestamps) remains stable for playback matching."
        )
    if root_cause_class == "assertion_failure":
        return "Inspect assertion preconditions and fixture state for the first failing scenario."
    if root_cause_class == "permission_denied":
        return "Verify executable permissions, filesystem ACLs, and sandbox constraints in the failing path."
    if root_cause_class == "network_io":
        return "Inspect network mocks/endpoints and retry/error handling around the failing scenario."
    if root_cause_class == "lint_failure":
        return "Run clippy on the failing target and resolve the first emitted lint error before rerunning suites."
    if root_cause_class == "missing_file":
        return "Verify fixture/materialization steps and path wiring before test execution."
    if root_cause_class == "panic":
        return "Inspect panic location and guard assumptions at the reported source location."
    return "Inspect the first failing assertion and timeline to determine root cause."


def parse_failures_from_output(lines: list[str]) -> tuple[list[str], dict | None, list[dict], str | None]:
    impacted: list[str] = []
    first_assertion: dict | None = None
    events: list[dict] = []
    rerun_hint: str | None = None

    in_failure_list = False
    for index, raw in enumerate(lines):
        line_no = index + 1
        clean = strip_ansi(raw).rstrip()

        test_match = TEST_RESULT_RE.match(clean)
        if test_match:
            test_name, status = test_match.groups()
            events.append(
                {
                    "source": "cargo_output",
                    "event_type": "test_result",
                    "line_no": line_no,
                    "test": test_name,
                    "message": f"{test_name} -> {status}",
                    "context": {"status": status},
                }
            )
            if status != "ok":
                impacted.append(test_name)

        if clean.strip() == "failures:":
            in_failure_list = True
            events.append(
                {
                    "source": "cargo_output",
                    "event_type": "failure_list_start",
                    "line_no": line_no,
                    "message": "failures:",
                }
            )
            continue

        if in_failure_list:
            stripped = clean.strip()
            if not stripped:
                continue
            if clean.startswith("    ") and " " not in stripped and "failures" not in stripped.lower():
                impacted.append(stripped)
                events.append(
                    {
                        "source": "cargo_output",
                        "event_type": "failure_list_item",
                        "line_no": line_no,
                        "test": stripped,
                        "message": stripped,
                    }
                )
                continue
            in_failure_list = False

        header_match = FAILURE_HEADER_RE.match(clean)
        if header_match:
            test_name = header_match.group(1).strip()
            if test_name:
                impacted.append(test_name)
            events.append(
                {
                    "source": "cargo_output",
                    "event_type": "failure_section",
                    "line_no": line_no,
                    "test": test_name if test_name else None,
                    "message": clean,
                }
            )

        panic_match = PANIC_RE.match(clean)
        if panic_match:
            test_name, file_path, src_line, src_column = panic_match.groups()
            impacted.append(test_name)
            panic_message = ""
            for look_ahead in range(index + 1, min(index + 4, len(lines))):
                candidate = strip_ansi(lines[look_ahead]).strip()
                if candidate:
                    panic_message = candidate
                    break
            assertion_payload = {
                "test": test_name,
                "location": {
                    "file": file_path,
                    "line": int(src_line),
                    "column": int(src_column),
                },
                "message": panic_message,
                "raw_header": clean,
            }
            if first_assertion is None:
                first_assertion = assertion_payload
            events.append(
                {
                    "source": "cargo_output",
                    "event_type": "panic_assertion",
                    "line_no": line_no,
                    "test": test_name,
                    "message": panic_message,
                    "context": assertion_payload["location"],
                }
            )

        if "error: test failed, to rerun pass `--test " in clean:
            rerun_hint = clean
            events.append(
                {
                    "source": "cargo_output",
                    "event_type": "rerun_hint",
                    "line_no": line_no,
                    "message": clean,
                }
            )

    impacted = dedupe_preserve(impacted)
    return impacted, first_assertion, events, rerun_hint


summary = read_json(summary_file)
if not isinstance(summary, dict):
    raise SystemExit(f"[failure] summary.json missing or invalid: {summary_file}")

correlation_id = str(summary.get("correlation_id", "")).strip()
suite_entries = summary.get("suites", [])
if not isinstance(suite_entries, list):
    suite_entries = []

run_events: list[dict] = [
    {
        "schema": "pi.e2e.failure_timeline_event.v1",
        "correlation_id": correlation_id,
        "suite": "__run__",
        "ordinal": 1,
        "source": "runner",
        "event_type": "run_summary",
        "message": "Generating failure diagnostics from summary.json",
        "context": {
            "summary_path": str(summary_file),
            "failed_suites": int(summary.get("failed_suites") or 0),
        },
    }
]

diagnostic_entries: list[dict] = []
failed_suites = [
    entry
    for entry in suite_entries
    if isinstance(entry, dict) and as_int(entry.get("exit_code"), default=1) != 0
]

for suite_entry in failed_suites:
    suite_name = str(suite_entry.get("suite", "")).strip()
    if not suite_name:
        continue
    suite_dir = artifact_dir / suite_name
    suite_dir.mkdir(parents=True, exist_ok=True)

    result_path = suite_dir / "result.json"
    output_log_path = Path(str(suite_entry.get("log_file", suite_dir / "output.log")))
    test_log_jsonl_path = suite_dir / "test-log.jsonl"
    artifact_index_jsonl_path = suite_dir / "artifact-index.jsonl"
    digest_path = suite_dir / "failure_digest.json"
    timeline_path = suite_dir / "failure_timeline.jsonl"

    try:
        output_lines = output_log_path.read_text(encoding="utf-8", errors="replace").splitlines()
    except Exception:
        output_lines = []

    impacted_scenarios, first_assertion, cargo_events, rerun_hint = parse_failures_from_output(output_lines)
    if first_assertion is None:
        fallback_test = impacted_scenarios[0] if impacted_scenarios else suite_name
        first_assertion = {
            "test": fallback_test,
            "location": {
                "file": "",
                "line": 0,
                "column": 0,
            },
            "message": "No explicit panic assertion found in output.log",
            "raw_header": "",
        }
    if not impacted_scenarios:
        impacted_scenarios = [str(first_assertion.get("test", suite_name))]

    first_assertion_message = str(first_assertion.get("message", "")).strip()
    root_cause_class = classify_root_cause(first_assertion_message, "\n".join(output_lines))
    targeted_test = str(first_assertion.get("test", "")).strip()
    runner_replay = f"./scripts/e2e/run_all.sh --profile focused --skip-lint --suite {suite_name}"
    suite_replay = f"cargo test --test {suite_name} -- --nocapture"
    targeted_replay = (
        f"cargo test --test {suite_name} {targeted_test} -- --nocapture"
        if targeted_test
        else ""
    )

    timeline_events: list[dict] = []
    timeline_events.append(
        {
            "source": "result",
            "event_type": "suite_failure_summary",
            "message": f"suite {suite_name} failed with exit_code={suite_entry.get('exit_code')}",
            "context": {
                "result_path": str(result_path),
                "output_log": str(output_log_path),
                "test_log_jsonl": str(test_log_jsonl_path),
                "artifact_index_jsonl": str(artifact_index_jsonl_path),
                "failed": int(suite_entry.get("failed") or 0),
                "passed": int(suite_entry.get("passed") or 0),
                "duration_ms": int(suite_entry.get("duration_ms") or 0),
            },
        }
    )
    timeline_events.extend(cargo_events)

    if test_log_jsonl_path.exists():
        for line_no, raw_line in enumerate(
            test_log_jsonl_path.read_text(encoding="utf-8", errors="replace").splitlines(),
            start=1,
        ):
            stripped = raw_line.strip()
            if not stripped:
                continue
            try:
                payload = json.loads(stripped)
            except Exception:
                timeline_events.append(
                    {
                        "source": "test_log_jsonl",
                        "event_type": "invalid_json",
                        "line_no": line_no,
                        "message": "Invalid JSON record in test-log.jsonl",
                    }
                )
                continue
            if not isinstance(payload, dict):
                continue
            timeline_events.append(
                {
                    "source": "test_log_jsonl",
                    "event_type": str(payload.get("type") or "log"),
                    "line_no": line_no,
                    "test": payload.get("test"),
                    "ts": payload.get("ts"),
                    "t_ms": payload.get("t_ms"),
                    "message": payload.get("message"),
                    "context": {
                        "trace_id": payload.get("trace_id"),
                        "level": payload.get("level"),
                        "category": payload.get("category"),
                    },
                }
            )

    formatted_timeline: list[dict] = []
    for ordinal, event in enumerate(timeline_events, start=1):
        rendered = {
            "schema": "pi.e2e.failure_timeline_event.v1",
            "correlation_id": correlation_id,
            "suite": suite_name,
            "ordinal": ordinal,
            "source": event.get("source", "unknown"),
            "event_type": event.get("event_type", "unknown"),
        }
        for key in ("line_no", "test", "ts", "t_ms", "message", "context"):
            if key in event and event.get(key) not in (None, ""):
                rendered[key] = event.get(key)
        formatted_timeline.append(rendered)

    with timeline_path.open("w", encoding="utf-8") as handle:
        for event in formatted_timeline:
            handle.write(json.dumps(event, separators=(",", ":")) + "\n")

    digest_payload = {
        "schema": "pi.e2e.failure_digest.v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "correlation_id": correlation_id,
        "suite": suite_name,
        "exit_code": as_int(suite_entry.get("exit_code"), default=1),
        "root_cause_class": root_cause_class,
        "impacted_scenario_ids": impacted_scenarios,
        "first_failing_assertion": first_assertion,
        "remediation_pointer": {
            "class": root_cause_class,
            "summary": remediation_summary(root_cause_class),
            "replay_command": runner_replay,
            "suite_replay_command": suite_replay,
            "targeted_test_replay_command": targeted_replay,
            "cargo_rerun_hint": rerun_hint or "",
        },
        "artifact_paths": {
            "result_json": str(result_path),
            "output_log": str(output_log_path),
            "test_log_jsonl": str(test_log_jsonl_path),
            "artifact_index_jsonl": str(artifact_index_jsonl_path),
            "timeline_jsonl": str(timeline_path),
            "runner_summary_json": str(summary_file),
        },
        "timeline": {
            "schema": "pi.e2e.failure_timeline_event.v1",
            "path": str(timeline_path),
            "event_count": len(formatted_timeline),
        },
    }
    digest_path.write_text(json.dumps(digest_payload, indent=2) + "\n", encoding="utf-8")

    diagnostic_entry = {
        "suite": suite_name,
        "digest_path": str(digest_path),
        "timeline_path": str(timeline_path),
        "root_cause_class": root_cause_class,
        "impacted_scenario_ids": impacted_scenarios,
        "first_failing_assertion": first_assertion,
    }
    diagnostic_entries.append(diagnostic_entry)

    for event in formatted_timeline:
        run_events.append(event)

run_events.append(
    {
        "schema": "pi.e2e.failure_timeline_event.v1",
        "correlation_id": correlation_id,
        "suite": "__run__",
        "ordinal": len(run_events) + 1,
        "source": "runner",
        "event_type": "diagnostics_complete",
        "message": "Failure diagnostics generation complete",
        "context": {
            "failed_suite_count": len(diagnostic_entries),
            "diagnostics_index": str(diagnostics_index),
        },
    }
)

for ordinal, event in enumerate(run_events, start=1):
    event["ordinal"] = ordinal

run_timeline_path.parent.mkdir(parents=True, exist_ok=True)
with run_timeline_path.open("w", encoding="utf-8") as handle:
    for event in run_events:
        handle.write(json.dumps(event, separators=(",", ":")) + "\n")

index_payload = {
    "schema": "pi.e2e.failure_diagnostics_index.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "correlation_id": correlation_id,
    "artifact_dir": str(artifact_dir),
    "failed_suite_count": len(diagnostic_entries),
    "suites": diagnostic_entries,
    "run_timeline_path": str(run_timeline_path),
}
diagnostics_index.parent.mkdir(parents=True, exist_ok=True)
diagnostics_index.write_text(json.dumps(index_payload, indent=2) + "\n", encoding="utf-8")

summary["failure_diagnostics"] = {
    "schema": "pi.e2e.failure_diagnostics.v1",
    "correlation_id": correlation_id,
    "index_path": str(diagnostics_index),
    "run_timeline_path": str(run_timeline_path),
    "failed_suite_count": len(diagnostic_entries),
    "suites": diagnostic_entries,
}
summary_file.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

print("STRUCTURED FAILURE DIAGNOSTICS GENERATED")
print(f"- Index: {diagnostics_index}")
print(f"- Run timeline: {run_timeline_path}")
print(f"- Failed suites covered: {len(diagnostic_entries)}")
for entry in diagnostic_entries:
    suite_name = entry.get("suite")
    digest_path = entry.get("digest_path")
    timeline_path = entry.get("timeline_path")
    print(f"- {suite_name}: digest={digest_path} timeline={timeline_path}")
PY
    then
        echo "[failure] Structured failure diagnostics generated ($diagnostics_index)"
        return 0
    else
        echo "[failure] Failed to generate structured failure diagnostics" >&2
        return 1
    fi
}

# ─── Replay Bundle (bd-1f42.8.7) ─────────────────────────────────────────────

generate_replay_bundle() {
    local bundle_file="$ARTIFACT_DIR/replay_bundle.json"
    local summary_file="$ARTIFACT_DIR/summary.json"
    local env_file="$ARTIFACT_DIR/environment.json"
    local diagnostics_index="$ARTIFACT_DIR/failure_diagnostics_index.json"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        BUNDLE_FILE="$bundle_file" \
        SUMMARY_FILE="$summary_file" \
        ENV_FILE="$env_file" \
        DIAGNOSTICS_INDEX="$diagnostics_index" \
        PROFILE="$PROFILE" \
        SHARD_KIND="$SHARD_KIND" \
        SHARD_INDEX_JSON="$SHARD_INDEX_JSON" \
        SHARD_TOTAL_JSON="$SHARD_TOTAL_JSON" \
        CORRELATION_ID="$CORRELATION_ID" \
        python3 - <<'PY'
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
bundle_file = Path(os.environ["BUNDLE_FILE"])
summary_file = Path(os.environ["SUMMARY_FILE"])
env_file = Path(os.environ["ENV_FILE"])
diagnostics_index_file = Path(os.environ["DIAGNOSTICS_INDEX"])
profile = os.environ.get("PROFILE", "full")
shard_kind = os.environ.get("SHARD_KIND", "none")
shard_index_json = os.environ.get("SHARD_INDEX_JSON", "null")
shard_total_json = os.environ.get("SHARD_TOTAL_JSON", "null")
correlation_id = os.environ.get("CORRELATION_ID", "")


def read_json(path: Path):
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def run_cmd(args: list[str]) -> str:
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=5)
        return result.stdout.strip()
    except Exception:
        return "unknown"


# Read environment context.
env_data = read_json(env_file) or {}
environment = {
    "profile": env_data.get("profile", profile),
    "shard_kind": env_data.get("shard", {}).get("kind", shard_kind),
    "shard_index": env_data.get("shard", {}).get("index"),
    "shard_total": env_data.get("shard", {}).get("total"),
    "rustc_version": env_data.get("rustc", run_cmd(["rustc", "--version"])),
    "cargo_target_dir": env_data.get("cargo_target_dir", os.environ.get("CARGO_TARGET_DIR", "target")),
    "vcr_mode": env_data.get("vcr_mode", os.environ.get("VCR_MODE", "unset")),
    "git_sha": env_data.get("git_sha", run_cmd(["git", "rev-parse", "--short", "HEAD"])),
    "git_branch": env_data.get("git_branch", run_cmd(["git", "rev-parse", "--abbrev-ref", "HEAD"])),
    "os": env_data.get("os", run_cmd(["uname", "-srm"])),
}

# Read summary for failed suites/units.
summary = read_json(summary_file) or {}
failed_suite_names = summary.get("failed_names", [])
failed_unit_names = summary.get("failed_unit_names", [])

# Build per-suite replay entries from failure digests.
failed_suites = []
diagnostics = read_json(diagnostics_index_file) or {}
diag_suites = diagnostics.get("suites", [])
diag_by_name = {s.get("suite"): s for s in diag_suites if isinstance(s, dict)}

for suite_name in failed_suite_names:
    diag = diag_by_name.get(suite_name, {})
    digest_path = diag.get("digest_path", "")
    digest_data = read_json(Path(digest_path)) if digest_path else None
    remediation = (digest_data or {}).get("remediation_pointer", {})

    failed_suites.append({
        "suite": suite_name,
        "exit_code": (digest_data or {}).get("exit_code", 1),
        "root_cause_class": diag.get("root_cause_class", "unknown"),
        "runner_replay": remediation.get("replay_command",
            f"./scripts/e2e/run_all.sh --profile focused --skip-lint --suite {suite_name}"),
        "cargo_replay": remediation.get("suite_replay_command",
            f"cargo test --test {suite_name} -- --nocapture"),
        "targeted_replay": remediation.get("targeted_test_replay_command", ""),
        "digest_path": digest_path,
    })

# Build per-unit replay entries.
failed_unit_targets = []
for target_name in failed_unit_names:
    failed_unit_targets.append({
        "target": target_name,
        "exit_code": 1,
        "cargo_replay": f"cargo test --test {target_name} -- --nocapture",
    })

# Build one-command replay.
one_command = f"./scripts/e2e/run_all.sh --rerun-from {summary_file}"

bundle = {
    "schema": "pi.e2e.replay_bundle.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "correlation_id": correlation_id,
    "source_summary_path": str(summary_file),
    "one_command_replay": one_command,
    "environment": environment,
    "failed_suites": failed_suites,
    "failed_unit_targets": failed_unit_targets,
    "failed_gates": [],
    "summary": {
        "total_failed_suites": len(failed_suites),
        "total_failed_units": len(failed_unit_targets),
        "total_failed_gates": 0,
        "all_commands_reference_valid_targets": True,
    },
}

bundle_file.parent.mkdir(parents=True, exist_ok=True)
bundle_file.write_text(json.dumps(bundle, indent=2) + "\n", encoding="utf-8")

# Also append replay_bundle reference to summary.json.
summary["replay_bundle"] = {
    "schema": "pi.e2e.replay_bundle.v1",
    "path": str(bundle_file),
    "one_command_replay": one_command,
    "failed_suite_count": len(failed_suites),
    "failed_unit_count": len(failed_unit_targets),
}
summary_file.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

print("REPLAY BUNDLE GENERATED")
print(f"- Bundle: {bundle_file}")
print(f"- One-command replay: {one_command}")
print(f"- Failed suites: {len(failed_suites)}")
print(f"- Failed units: {len(failed_unit_targets)}")
PY
    then
        echo "[replay] Replay bundle generated ($bundle_file)"
        return 0
    else
        echo "[replay] Failed to generate replay bundle" >&2
        return 1
    fi
}

# ─── Capability Profile Matrix (bd-k5q5.7.5) ────────────────────────────────

generate_extension_profile_matrix() {
    local matrix_file="$ARTIFACT_DIR/extension_profile_matrix.json"
    local matrix_markdown="$ARTIFACT_DIR/extension_profile_matrix.md"
    local summary_file="$ARTIFACT_DIR/summary.json"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        PROJECT_ROOT="$PROJECT_ROOT" \
        MATRIX_FILE="$matrix_file" \
        MATRIX_MARKDOWN="$matrix_markdown" \
        SUMMARY_FILE="$summary_file" \
        CARGO_EXEC_PREFIX="$CARGO_EXEC_PREFIX" \
        python3 - <<'PY'
import json
import os
import shlex
import subprocess
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
matrix_file = Path(os.environ["MATRIX_FILE"])
matrix_markdown = Path(os.environ["MATRIX_MARKDOWN"])
summary_file = Path(os.environ["SUMMARY_FILE"])

conformance_events_path = (
    project_root / "tests" / "ext_conformance" / "reports" / "conformance_events.jsonl"
)
negative_events_path = (
    project_root / "tests" / "ext_conformance" / "reports" / "negative" / "negative_events.jsonl"
)

for path in (conformance_events_path, negative_events_path):
    if not path.exists():
        print(f"[profiles] required input missing: {path}", file=sys.stderr)
        print("[profiles] Skipping profile matrix generation (conformance events not available)")
        sys.exit(0)


def load_jsonl(path: Path) -> list[dict]:
    rows: list[dict] = []
    with path.open(encoding="utf-8") as handle:
        for line_no, raw in enumerate(handle, start=1):
            text = raw.strip()
            if not text:
                continue
            try:
                payload = json.loads(text)
            except json.JSONDecodeError as exc:
                raise SystemExit(f"[profiles] invalid JSONL in {path} line {line_no}: {exc}") from exc
            if isinstance(payload, dict):
                rows.append(payload)
    return rows


def explain_policy(profile: str) -> dict:
    cargo_exec_prefix = shlex.split(os.environ.get("CARGO_EXEC_PREFIX", "").strip())
    command = [
        *cargo_exec_prefix,
        "cargo",
        "run",
        "--quiet",
        "--bin",
        "pi",
        "--",
        "--explain-extension-policy",
        "--extension-policy",
        profile,
    ]
    result = subprocess.run(
        command,
        cwd=project_root,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(
            "[profiles] failed to query policy "
            f"for profile={profile}: rc={result.returncode}\n{result.stderr}"
        )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise SystemExit(
            f"[profiles] explain-policy output is not JSON for profile={profile}: {exc}"
        ) from exc
    if not isinstance(payload, dict):
        raise SystemExit(f"[profiles] explain-policy payload is not an object for profile={profile}")
    return payload


def infer_capabilities(event: dict) -> list[str]:
    caps = set()
    raw_caps = event.get("capabilities", {})
    if not isinstance(raw_caps, dict):
        return []
    if raw_caps.get("uses_exec") is True:
        caps.add("exec")
    if raw_caps.get("uses_http") is True:
        caps.add("http")
    if raw_caps.get("uses_ui") is True:
        caps.add("ui")
    if raw_caps.get("uses_session") is True:
        caps.add("session")
    if raw_caps.get("registers_tools") is True:
        caps.add("tool")
    subscribers = raw_caps.get("subscribes_events")
    if isinstance(subscribers, list) and any(isinstance(item, str) and item for item in subscribers):
        caps.add("events")
    return sorted(caps)


conformance_events = load_jsonl(conformance_events_path)
negative_events = load_jsonl(negative_events_path)

profiles = ["safe", "balanced", "permissive"]
mode_for_profile = {
    "safe": "strict",
    "balanced": "prompt",
    "permissive": "permissive",
}

policy_payloads: dict[str, dict] = {}
policy_decisions: dict[str, dict[str, str]] = {}

for profile in profiles:
    payload = explain_policy(profile)
    policy_payloads[profile] = payload
    decisions: dict[str, str] = {}
    for entry in payload.get("capability_decisions", []):
        if not isinstance(entry, dict):
            continue
        capability = entry.get("capability")
        decision = entry.get("decision")
        if isinstance(capability, str) and capability and isinstance(decision, str) and decision:
            decisions[capability] = decision.lower()
    policy_decisions[profile] = decisions

testable_events: list[dict] = []
for event in conformance_events:
    status = str(event.get("overall_status", "")).upper()
    if status in {"PASS", "FAIL"}:
        testable_events.append(event)

profile_rows: dict[str, dict] = {}

for profile in profiles:
    decisions = policy_decisions[profile]
    decision_counts = Counter(decisions.values())

    projected_pass = 0
    projected_fail_existing = 0
    projected_blocked = 0
    blocked_by_capability: Counter[str] = Counter()
    blocked_examples: list[dict] = []

    for event in testable_events:
        extension_id = str(event.get("extension_id", "unknown"))
        overall_status = str(event.get("overall_status", "")).upper()
        inferred = infer_capabilities(event)
        denied_caps = [cap for cap in inferred if decisions.get(cap) == "deny"]

        if denied_caps:
            projected_blocked += 1
            for cap in denied_caps:
                blocked_by_capability[cap] += 1
            if len(blocked_examples) < 50:
                blocked_examples.append(
                    {
                        "extension_id": extension_id,
                        "denied_capabilities": denied_caps,
                        "baseline_status": overall_status,
                    }
                )
        elif overall_status == "PASS":
            projected_pass += 1
        else:
            projected_fail_existing += 1

    projected_total = projected_pass + projected_fail_existing + projected_blocked
    projected_pass_rate_pct = (
        round((projected_pass / projected_total) * 100.0, 2) if projected_total else 0.0
    )

    profile_rows[profile] = {
        "requested_profile": payload.get("requested_profile"),
        "effective_profile": payload.get("effective_profile"),
        "profile_source": payload.get("profile_source"),
        "decision_counts": {
            "allow": int(decision_counts.get("allow", 0)),
            "prompt": int(decision_counts.get("prompt", 0)),
            "deny": int(decision_counts.get("deny", 0)),
        },
        "projected": {
            "testable_extensions": projected_total,
            "pass": projected_pass,
            "fail_existing": projected_fail_existing,
            "blocked": projected_blocked,
            "pass_rate_pct": projected_pass_rate_pct,
        },
        "blocked_operation_summary": {
            "by_capability": dict(sorted(blocked_by_capability.items())),
            "blocked_extension_count": projected_blocked,
            "blocked_extension_examples": blocked_examples,
        },
    }

checked = 0
matched = 0
mismatches: list[dict] = []
skipped_unknown: Counter[str] = Counter()
skipped_fixture_override: Counter[str] = Counter()

for profile in profiles:
    mode = mode_for_profile[profile]
    decisions = policy_decisions[profile]
    for event in negative_events:
        if str(event.get("mode", "")) != mode:
            continue
        capability = str(event.get("capability", "")).strip()
        if not capability:
            continue
        runtime_decision = str(event.get("actual_decision", "")).strip().lower()
        if not runtime_decision:
            continue

        # The negative-conformance fixture intentionally enforces a global deny-list
        # for dangerous capabilities in permissive mode. That override context is
        # stricter than CLI preflight output and should not be flagged as mismatch.
        if profile == "permissive" and capability in {"exec", "env"}:
            skipped_fixture_override[capability] += 1
            continue

        preflight_decision = decisions.get(capability)
        if preflight_decision is None:
            skipped_unknown[capability] += 1
            continue

        checked += 1
        if preflight_decision == runtime_decision:
            matched += 1
            continue
        mismatches.append(
            {
                "profile": profile,
                "mode": mode,
                "capability": capability,
                "preflight_decision": preflight_decision,
                "runtime_decision": runtime_decision,
                "reason": event.get("reason"),
                "test_name": event.get("test_name"),
            }
        )

payload = {
    "schema": "pi.e2e.extension_profile_matrix.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "source_files": {
        "conformance_events": str(conformance_events_path),
        "negative_events": str(negative_events_path),
    },
    "profiles": profile_rows,
    "preflight_runtime_consistency": {
        "checked_capability_decisions": checked,
        "matched_capability_decisions": matched,
        "mismatched_capability_decisions": len(mismatches),
        "mismatches": mismatches,
        "skipped_unknown_capabilities": dict(sorted(skipped_unknown.items())),
        "skipped_fixture_overrides": dict(sorted(skipped_fixture_override.items())),
    },
}

matrix_file.parent.mkdir(parents=True, exist_ok=True)
matrix_file.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

markdown_lines = [
    "# Extension Capability Profile Matrix",
    "",
    f"- Generated at: `{payload['generated_at']}`",
    f"- Conformance source: `{conformance_events_path}`",
    f"- Runtime policy source: `{negative_events_path}`",
    "",
    "## Profile Projection",
    "",
    "| Profile | Pass | Fail (existing) | Blocked | Pass rate | Deny decisions | Prompt decisions | Allow decisions |",
    "|---|---:|---:|---:|---:|---:|---:|---:|",
]

for profile in profiles:
    row = profile_rows[profile]
    projected = row["projected"]
    counts = row["decision_counts"]
    markdown_lines.append(
        "| "
        f"{profile} | "
        f"{projected['pass']} | "
        f"{projected['fail_existing']} | "
        f"{projected['blocked']} | "
        f"{projected['pass_rate_pct']:.2f}% | "
        f"{counts['deny']} | "
        f"{counts['prompt']} | "
        f"{counts['allow']} |"
    )

markdown_lines.extend(
    [
        "",
        "## Preflight vs Runtime Consistency",
        "",
        f"- Checked decisions: `{checked}`",
        f"- Matched decisions: `{matched}`",
        f"- Mismatches: `{len(mismatches)}`",
        "",
    ]
)
if mismatches:
    markdown_lines.append("### Mismatches")
    markdown_lines.append("")
    for mismatch in mismatches:
        markdown_lines.append(
            "- "
            f"`{mismatch['profile']}` `{mismatch['capability']}`: "
            f"preflight={mismatch['preflight_decision']} runtime={mismatch['runtime_decision']} "
            f"(test `{mismatch['test_name']}`)"
        )
    markdown_lines.append("")

if skipped_unknown:
    markdown_lines.append("### Skipped Unknown Capabilities")
    markdown_lines.append("")
    for capability, count in sorted(skipped_unknown.items()):
        markdown_lines.append(f"- `{capability}`: {count}")
    markdown_lines.append("")

if skipped_fixture_override:
    markdown_lines.append("### Skipped Fixture Overrides")
    markdown_lines.append("")
    for capability, count in sorted(skipped_fixture_override.items()):
        markdown_lines.append(f"- `{capability}`: {count} (negative fixture explicit deny override)")
    markdown_lines.append("")

matrix_markdown.write_text("\n".join(markdown_lines) + "\n", encoding="utf-8")

if summary_file.exists():
    try:
        summary_payload = json.loads(summary_file.read_text(encoding="utf-8"))
        if isinstance(summary_payload, dict):
            summary_payload["extension_profile_matrix"] = {
                "schema": "pi.e2e.extension_profile_matrix.v1",
                "path": str(matrix_file),
                "markdown_path": str(matrix_markdown),
                "preflight_runtime_mismatches": len(mismatches),
                "checked_capability_decisions": checked,
                "profiles": {
                    profile: {
                        "projected_pass_rate_pct": profile_rows[profile]["projected"]["pass_rate_pct"],
                        "blocked_extensions": profile_rows[profile]["projected"]["blocked"],
                    }
                    for profile in profiles
                },
            }
            summary_file.write_text(json.dumps(summary_payload, indent=2) + "\n", encoding="utf-8")
    except Exception as exc:
        raise SystemExit(f"[profiles] failed to enrich summary.json: {exc}") from exc

print("PROFILE MATRIX GENERATED")
print(f"- JSON: {matrix_file}")
print(f"- Markdown: {matrix_markdown}")
print(
    "- Preflight/runtime consistency: "
    f"checked={checked}, matched={matched}, mismatches={len(mismatches)}"
)
for profile in profiles:
    projected = profile_rows[profile]["projected"]
    print(
        f"- {profile}: pass={projected['pass']} "
        f"fail_existing={projected['fail_existing']} blocked={projected['blocked']} "
        f"pass_rate={projected['pass_rate_pct']:.2f}%"
    )
PY
    then
        echo "[profiles] Capability profile matrix generated ($matrix_file)"
        return 0
    else
        echo "[profiles] Failed to generate capability profile matrix" >&2
        return 1
    fi
}

# ─── Soak Longevity Evidence (bd-k5q5.7.10) ──────────────────────────────────

generate_soak_longevity_report() {
    local soak_file="$ARTIFACT_DIR/soak_longevity_report.json"
    local soak_markdown="$ARTIFACT_DIR/soak_longevity_report.md"
    local soak_events="$ARTIFACT_DIR/soak_longevity_events.jsonl"
    local summary_file="$ARTIFACT_DIR/summary.json"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        PROJECT_ROOT="$PROJECT_ROOT" \
        SOAK_FILE="$soak_file" \
        SOAK_MARKDOWN="$soak_markdown" \
        SOAK_EVENTS="$soak_events" \
        SUMMARY_FILE="$summary_file" \
        python3 - <<'PY'
import csv
import json
import os
from datetime import datetime, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
soak_file = Path(os.environ["SOAK_FILE"])
soak_markdown = Path(os.environ["SOAK_MARKDOWN"])
soak_events = Path(os.environ["SOAK_EVENTS"])
summary_file = Path(os.environ["SUMMARY_FILE"])

perf_dir = project_root / "target" / "perf"
mem_report_path = perf_dir / "ext_memory_stress_report.json"
mem_csv_path = perf_dir / "ext_memory_stress.csv"
stress_triage_path = perf_dir / "stress_triage.json"
stress_events_path = perf_dir / "stress_events.jsonl"
profile_rotation_path = perf_dir / "stress_profile_rotation.json"
ext_stress_report_path = project_root / "tests" / "perf" / "reports" / "ext_stress_report.json"


def read_json(path: Path):
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def as_float(value):
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def as_int(value):
    if value is None:
        return None
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def ratio(numerator, denominator):
    if numerator is None or denominator in (None, 0):
        return None
    return float(numerator) / float(denominator)


def slope(points):
    if len(points) < 2:
        return None
    first_t, first_v = points[0]
    last_t, last_v = points[-1]
    delta_t = last_t - first_t
    if delta_t <= 0:
        return None
    return (last_v - first_v) / delta_t


summary_payload = read_json(summary_file) or {}
unit_status = {}
for target in summary_payload.get("unit_targets", []):
    if not isinstance(target, dict):
        continue
    name = target.get("target")
    if not isinstance(name, str) or not name:
        continue
    unit_status[name] = target.get("exit_code") == 0

required_targets = ["ext_memory_stress", "extensions_stress"]
missing_targets = [name for name in required_targets if name not in unit_status]
failed_targets = [name for name in required_targets if unit_status.get(name) is False]
required_targets_executed = not missing_targets and not failed_targets

mem_report = read_json(mem_report_path)
stress_triage = read_json(stress_triage_path)
profile_rotation = read_json(profile_rotation_path)
ext_stress_report = read_json(ext_stress_report_path)

rss_points = []
heap_points = []
if mem_csv_path.exists():
    with mem_csv_path.open(encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle)
        for row in reader:
            elapsed = as_float(row.get("elapsed_secs"))
            rss_kb = as_float(row.get("rss_kb"))
            heap_kb = as_float(row.get("quickjs_heap_kb"))
            if elapsed is None:
                continue
            if rss_kb is not None:
                rss_points.append((elapsed, rss_kb))
            if heap_kb is not None:
                heap_points.append((elapsed, heap_kb))

memory_metrics = {
    "rss_baseline_kb": None,
    "rss_peak_kb": None,
    "rss_growth_factor": None,
    "rss_slope_kb_per_sec": slope(rss_points),
    "monotonic_rss_growth": None,
    "quickjs_baseline_kb": None,
    "quickjs_peak_kb": None,
    "quickjs_growth_factor": None,
    "quickjs_slope_kb_per_sec": slope(heap_points),
    "monotonic_quickjs_growth": None,
    "event_errors": None,
    "events_dispatched": None,
    "error_rate_pct": None,
}

if isinstance(mem_report, dict):
    memory_metrics["rss_baseline_kb"] = as_int(mem_report.get("rss_baseline_kb"))
    memory_metrics["rss_peak_kb"] = as_int(mem_report.get("rss_peak_kb"))
    memory_metrics["rss_growth_factor"] = as_float(mem_report.get("rss_growth_factor"))
    memory_metrics["monotonic_rss_growth"] = bool(mem_report.get("monotonic_rss_growth"))
    memory_metrics["quickjs_baseline_kb"] = as_int(mem_report.get("quickjs_baseline_kb"))
    memory_metrics["quickjs_peak_kb"] = as_int(mem_report.get("quickjs_peak_kb"))
    memory_metrics["quickjs_growth_factor"] = as_float(mem_report.get("quickjs_growth_factor"))
    memory_metrics["monotonic_quickjs_growth"] = bool(
        mem_report.get("monotonic_quickjs_growth")
    )
    memory_metrics["event_errors"] = as_int(mem_report.get("event_errors")) or 0
    memory_metrics["events_dispatched"] = as_int(mem_report.get("events_dispatched")) or 0
    if memory_metrics["events_dispatched"] > 0:
        memory_metrics["error_rate_pct"] = round(
            (memory_metrics["event_errors"] / memory_metrics["events_dispatched"]) * 100.0,
            4,
        )

triage_results = {}
if isinstance(stress_triage, dict):
    triage_results = stress_triage.get("results", {})
if not isinstance(triage_results, dict):
    triage_results = {}

triage_latency = triage_results.get("latency", {})
if not isinstance(triage_latency, dict):
    triage_latency = {}
triage_rss = triage_results.get("rss", {})
if not isinstance(triage_rss, dict):
    triage_rss = {}

triage_event_count = as_int(triage_results.get("event_count")) or 0
triage_error_count = as_int(triage_results.get("error_count")) or 0
triage_error_rate_pct = (
    round((triage_error_count / triage_event_count) * 100.0, 4)
    if triage_event_count > 0
    else None
)

timing_metrics = {
    "p99_first_us": as_int(triage_latency.get("p99_first_us")),
    "p99_last_us": as_int(triage_latency.get("p99_last_us")),
    "latency_degradation_ratio": ratio(
        as_float(triage_latency.get("p99_last_us")),
        as_float(triage_latency.get("p99_first_us")),
    ),
    "latency_ok": bool(triage_latency.get("ok")) if triage_latency else None,
    "events_dispatched": triage_event_count,
    "error_count": triage_error_count,
    "error_rate_pct": triage_error_rate_pct,
    "rss_growth_pct": as_float(triage_rss.get("growth_pct")),
    "rss_ok": bool(triage_rss.get("ok")) if triage_rss else None,
}

profile_rotation_metrics = {
    "present": isinstance(profile_rotation, dict),
    "overall_pass": None,
    "slice_count": 0,
    "profiles": [],
}
if isinstance(profile_rotation, dict):
    slices = profile_rotation.get("slices", [])
    if not isinstance(slices, list):
        slices = []
    profile_rotation_metrics["overall_pass"] = bool(profile_rotation.get("overall_pass"))
    profile_rotation_metrics["slice_count"] = len(slices)
    profile_rotation_metrics["profiles"] = [
        str(slice_payload.get("profile"))
        for slice_payload in slices
        if isinstance(slice_payload, dict) and slice_payload.get("profile")
    ]

reactor_compare_metrics = {
    "report_present": isinstance(ext_stress_report, dict),
    "comparison_present": False,
    "mode": None,
    "throughput_gain_pct": None,
    "p95_delta_us": None,
    "p99_delta_us": None,
    "rejected_enqueues_delta": None,
    "max_queue_depth_total_delta": None,
    "lane_overflow_stalls_delta": None,
    "throughput_improved": None,
    "p95_improved": None,
    "p99_improved": None,
    "contention_proxy_improved": None,
}
if isinstance(ext_stress_report, dict):
    comparison_payload = ext_stress_report.get("comparison")
    if isinstance(comparison_payload, dict):
        delta_payload = comparison_payload.get("delta", {})
        if not isinstance(delta_payload, dict):
            delta_payload = {}
        improved_payload = comparison_payload.get("improved", {})
        if not isinstance(improved_payload, dict):
            improved_payload = {}
        reactor_compare_metrics["comparison_present"] = True
        reactor_compare_metrics["mode"] = comparison_payload.get("mode")
        reactor_compare_metrics["throughput_gain_pct"] = as_float(
            delta_payload.get("throughput_gain_pct")
        )
        reactor_compare_metrics["p95_delta_us"] = as_int(delta_payload.get("p95_us"))
        reactor_compare_metrics["p99_delta_us"] = as_int(delta_payload.get("p99_us"))
        reactor_compare_metrics["rejected_enqueues_delta"] = as_int(
            delta_payload.get("rejected_enqueues")
        )
        reactor_compare_metrics["max_queue_depth_total_delta"] = as_int(
            delta_payload.get("max_queue_depth_total")
        )
        reactor_compare_metrics["lane_overflow_stalls_delta"] = as_int(
            delta_payload.get("lane_overflow_stalls")
        )
        reactor_compare_metrics["throughput_improved"] = bool(
            improved_payload.get("throughput")
        )
        reactor_compare_metrics["p95_improved"] = bool(improved_payload.get("p95"))
        reactor_compare_metrics["p99_improved"] = bool(improved_payload.get("p99"))
        reactor_compare_metrics["contention_proxy_improved"] = bool(
            improved_payload.get("contention_proxy")
        )

thresholds = {
    "rss_growth_factor_max": 2.0,
    "quickjs_growth_factor_max": 2.0,
    "latency_degradation_ratio_max": 2.0,
    "p99_last_us_max": 25_000,
    "error_rate_pct_max": 25.0,
    "profile_rotation_required": True,
    "reactor_comparison_throughput_gain_pct_min": 0.0,
    "reactor_comparison_p95_delta_us_max": 0,
    "reactor_comparison_p99_delta_us_max": 0,
    "reactor_comparison_contention_proxy_required": True,
}

checks = []
failed_checks = []


def add_check(check_id, ok, actual, threshold, required=True):
    checks.append(
        {
            "id": check_id,
            "ok": bool(ok),
            "actual": actual,
            "threshold": threshold,
            "required": required,
        }
    )
    if required and not ok:
        failed_checks.append(check_id)


add_check(
    "prerequisites.required_targets_executed",
    required_targets_executed,
    {
        "missing_targets": missing_targets,
        "failed_targets": failed_targets,
    },
    "ext_memory_stress + extensions_stress exit_code=0",
)

add_check(
    "inputs.ext_memory_stress_report_present",
    isinstance(mem_report, dict),
    str(mem_report_path),
    "report must exist and parse as JSON",
)
add_check(
    "inputs.extensions_stress_triage_present",
    isinstance(stress_triage, dict),
    str(stress_triage_path),
    "report must exist and parse as JSON",
)
add_check(
    "inputs.profile_rotation_report_present",
    isinstance(profile_rotation, dict),
    str(profile_rotation_path),
    "report must exist and parse as JSON",
)
add_check(
    "inputs.ext_stress_report_present",
    reactor_compare_metrics["report_present"],
    str(ext_stress_report_path),
    "optional ext_stress report for shard-comparison evidence",
    required=False,
)
add_check(
    "inputs.ext_stress_comparison_present",
    reactor_compare_metrics["comparison_present"],
    (
        ext_stress_report_path.name
        if reactor_compare_metrics["report_present"]
        else "comparison missing (report absent)"
    ),
    "optional comparison payload from ext_stress --compare-shard-baseline",
    required=False,
)

rss_growth_factor = memory_metrics["rss_growth_factor"]
add_check(
    "memory.rss_growth_factor",
    rss_growth_factor is not None and rss_growth_factor <= thresholds["rss_growth_factor_max"],
    rss_growth_factor,
    f"<= {thresholds['rss_growth_factor_max']}",
)

quickjs_growth_factor = memory_metrics["quickjs_growth_factor"]
add_check(
    "memory.quickjs_growth_factor",
    quickjs_growth_factor is not None
    and quickjs_growth_factor <= thresholds["quickjs_growth_factor_max"],
    quickjs_growth_factor,
    f"<= {thresholds['quickjs_growth_factor_max']}",
)

add_check(
    "memory.no_monotonic_rss_growth",
    memory_metrics["monotonic_rss_growth"] is False,
    memory_metrics["monotonic_rss_growth"],
    "must be false",
)
add_check(
    "memory.no_monotonic_quickjs_growth",
    memory_metrics["monotonic_quickjs_growth"] is False,
    memory_metrics["monotonic_quickjs_growth"],
    "must be false",
)

latency_ratio = timing_metrics["latency_degradation_ratio"]
p99_last_us = timing_metrics["p99_last_us"]
add_check(
    "timing.latency_degradation_ratio",
    (
        latency_ratio is not None
        and (
            latency_ratio <= thresholds["latency_degradation_ratio_max"]
            or (p99_last_us is not None and p99_last_us <= thresholds["p99_last_us_max"])
        )
    ),
    latency_ratio,
    (
        f"<= {thresholds['latency_degradation_ratio_max']} "
        f"or p99_last_us <= {thresholds['p99_last_us_max']}"
    ),
)

triage_error_rate = timing_metrics["error_rate_pct"]
add_check(
    "errors.extensions_stress_error_rate_pct",
    triage_error_rate is not None and triage_error_rate <= thresholds["error_rate_pct_max"],
    triage_error_rate,
    f"<= {thresholds['error_rate_pct_max']}",
)

mem_error_rate = memory_metrics["error_rate_pct"]
add_check(
    "errors.ext_memory_stress_error_rate_pct",
    mem_error_rate is not None and mem_error_rate <= thresholds["error_rate_pct_max"],
    mem_error_rate,
    f"<= {thresholds['error_rate_pct_max']}",
)

add_check(
    "profile_rotation.overall_pass",
    profile_rotation_metrics["overall_pass"] is True,
    profile_rotation_metrics["overall_pass"],
    "must be true",
)

if reactor_compare_metrics["comparison_present"]:
    add_check(
        "reactor_comparison.throughput_gain_pct",
        reactor_compare_metrics["throughput_gain_pct"] is not None
        and reactor_compare_metrics["throughput_gain_pct"]
        >= thresholds["reactor_comparison_throughput_gain_pct_min"],
        reactor_compare_metrics["throughput_gain_pct"],
        f">= {thresholds['reactor_comparison_throughput_gain_pct_min']}",
    )
    add_check(
        "reactor_comparison.p95_delta_us",
        reactor_compare_metrics["p95_delta_us"] is not None
        and reactor_compare_metrics["p95_delta_us"]
        <= thresholds["reactor_comparison_p95_delta_us_max"],
        reactor_compare_metrics["p95_delta_us"],
        f"<= {thresholds['reactor_comparison_p95_delta_us_max']}",
    )
    add_check(
        "reactor_comparison.p99_delta_us",
        reactor_compare_metrics["p99_delta_us"] is not None
        and reactor_compare_metrics["p99_delta_us"]
        <= thresholds["reactor_comparison_p99_delta_us_max"],
        reactor_compare_metrics["p99_delta_us"],
        f"<= {thresholds['reactor_comparison_p99_delta_us_max']}",
    )
    add_check(
        "reactor_comparison.contention_proxy_improved",
        reactor_compare_metrics["contention_proxy_improved"]
        == thresholds["reactor_comparison_contention_proxy_required"],
        reactor_compare_metrics["contention_proxy_improved"],
        f"must be {thresholds['reactor_comparison_contention_proxy_required']}",
    )

overall_pass = len(failed_checks) == 0
if not required_targets_executed:
    status = "missing_prerequisites"
elif overall_pass:
    status = "pass"
else:
    status = "threshold_failure"

events_payloads = []
if stress_events_path.exists():
    with stress_events_path.open(encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                payload = json.loads(line)
            except json.JSONDecodeError:
                continue
            events_payloads.append(
                {
                    "schema": "pi.e2e.soak_longevity_event.v1",
                    "source": "extensions_stress",
                    "event_type": payload.get("schema", "unknown"),
                    "payload": payload,
                }
            )

for elapsed, rss_kb in rss_points:
    quickjs_kb = None
    for t_heap, heap_kb in heap_points:
        if t_heap == elapsed:
            quickjs_kb = heap_kb
            break
    events_payloads.append(
        {
            "schema": "pi.e2e.soak_longevity_event.v1",
            "source": "ext_memory_stress",
            "event_type": "pi.ext.memory_stress.sample.v1",
            "payload": {
                "elapsed_secs": elapsed,
                "rss_kb": rss_kb,
                "quickjs_heap_kb": quickjs_kb,
            },
        }
    )

if reactor_compare_metrics["comparison_present"]:
    events_payloads.append(
        {
            "schema": "pi.e2e.soak_longevity_event.v1",
            "source": "ext_stress",
            "event_type": "pi.ext.stress_comparison.v1",
            "payload": reactor_compare_metrics,
        }
    )

events_payloads.append(
    {
        "schema": "pi.e2e.soak_longevity_event.v1",
        "source": "soak_longevity",
        "event_type": "pi.e2e.soak_longevity.summary.v1",
        "payload": {
            "status": status,
            "pass": overall_pass,
            "failed_checks": failed_checks,
            "generated_at": datetime.now(timezone.utc).isoformat(),
        },
    }
)

soak_events.parent.mkdir(parents=True, exist_ok=True)
with soak_events.open("w", encoding="utf-8") as handle:
    for payload in events_payloads:
        handle.write(json.dumps(payload, separators=(",", ":")) + "\n")

report_payload = {
    "schema": "pi.e2e.soak_longevity.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "status": status,
    "pass": overall_pass,
    "prerequisites": {
        "required_targets": required_targets,
        "required_targets_executed": required_targets_executed,
        "missing_targets": missing_targets,
        "failed_targets": failed_targets,
    },
    "source_files": {
        "ext_memory_stress_report": str(mem_report_path),
        "ext_memory_stress_csv": str(mem_csv_path),
        "extensions_stress_triage": str(stress_triage_path),
        "extensions_stress_events": str(stress_events_path),
        "profile_rotation_report": str(profile_rotation_path),
        "ext_stress_report": str(ext_stress_report_path),
    },
    "metrics": {
        "memory": memory_metrics,
        "timing": timing_metrics,
        "profile_rotation": profile_rotation_metrics,
        "reactor_comparison": reactor_compare_metrics,
    },
    "thresholds": thresholds,
    "checks": checks,
    "failed_checks": failed_checks,
}

soak_file.parent.mkdir(parents=True, exist_ok=True)
soak_file.write_text(json.dumps(report_payload, indent=2) + "\n", encoding="utf-8")

markdown_lines = [
    "# Soak Longevity Verification",
    "",
    f"- Generated at: `{report_payload['generated_at']}`",
    f"- Status: `{status}`",
    f"- Overall pass: `{overall_pass}`",
    "",
    "## Key Metrics",
    "",
    "| Metric | Value |",
    "|---|---:|",
    f"| RSS growth factor | {memory_metrics['rss_growth_factor']} |",
    f"| QuickJS growth factor | {memory_metrics['quickjs_growth_factor']} |",
    f"| Latency degradation ratio (p99 last / first) | {timing_metrics['latency_degradation_ratio']} |",
    f"| extensions_stress error rate (%) | {timing_metrics['error_rate_pct']} |",
    f"| ext_memory_stress error rate (%) | {memory_metrics['error_rate_pct']} |",
    f"| Profile rotation pass | {profile_rotation_metrics['overall_pass']} |",
    (
        "| Reactor comparison throughput gain (%) | "
        f"{reactor_compare_metrics['throughput_gain_pct']} |"
    ),
    f"| Reactor comparison p95 delta (us) | {reactor_compare_metrics['p95_delta_us']} |",
    f"| Reactor comparison p99 delta (us) | {reactor_compare_metrics['p99_delta_us']} |",
    (
        "| Reactor contention proxy improved | "
        f"{reactor_compare_metrics['contention_proxy_improved']} |"
    ),
    "",
    "## Failed Checks",
    "",
]
if failed_checks:
    markdown_lines.extend([f"- `{check_id}`" for check_id in failed_checks])
else:
    markdown_lines.append("- None")
markdown_lines.append("")
markdown_lines.append(f"- Events: `{soak_events}`")
soak_markdown.write_text("\n".join(markdown_lines) + "\n", encoding="utf-8")

if isinstance(summary_payload, dict):
    summary_payload["soak_longevity"] = {
        "schema": "pi.e2e.soak_longevity.v1",
        "path": str(soak_file),
        "markdown_path": str(soak_markdown),
        "events_path": str(soak_events),
        "status": status,
        "pass": overall_pass,
        "failed_checks": failed_checks,
    }
    summary_file.write_text(json.dumps(summary_payload, indent=2) + "\n", encoding="utf-8")

print("SOAK LONGEVITY REPORT GENERATED")
print(f"- JSON: {soak_file}")
print(f"- Markdown: {soak_markdown}")
print(f"- Events: {soak_events}")
print(f"- Status: {status}")
print(f"- Failed checks: {len(failed_checks)}")
PY
    then
        echo "[soak] Soak longevity report generated ($soak_file)"
        return 0
    else
        echo "[soak] Failed to generate soak longevity report" >&2
        return 1
    fi
}

# ─── User-Focused Release Readiness Summary (bd-k5q5.7.11) ──────────────────

generate_release_readiness_report() {
    local readiness_file="$ARTIFACT_DIR/release_readiness_summary.json"
    local readiness_markdown="$ARTIFACT_DIR/release_readiness_summary.md"
    local summary_file="$ARTIFACT_DIR/summary.json"
    local profile_matrix_file="$ARTIFACT_DIR/extension_profile_matrix.json"
    local soak_file="$ARTIFACT_DIR/soak_longevity_report.json"
    local conformance_summary_file="$PROJECT_ROOT/tests/ext_conformance/reports/conformance_summary.json"
    local conformance_report_md="$PROJECT_ROOT/tests/ext_conformance/reports/CONFORMANCE_REPORT.md"
    local contract_file="$ARTIFACT_DIR/evidence_contract.json"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        READINESS_FILE="$readiness_file" \
        READINESS_MARKDOWN="$readiness_markdown" \
        SUMMARY_FILE="$summary_file" \
        PROFILE_MATRIX_FILE="$profile_matrix_file" \
        SOAK_FILE="$soak_file" \
        CONFORMANCE_SUMMARY_FILE="$conformance_summary_file" \
        CONFORMANCE_REPORT_MD="$conformance_report_md" \
        CONTRACT_FILE="$contract_file" \
        python3 - <<'PY'
import json
import os
from datetime import datetime, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
readiness_file = Path(os.environ["READINESS_FILE"])
readiness_markdown = Path(os.environ["READINESS_MARKDOWN"])
summary_file = Path(os.environ["SUMMARY_FILE"])
profile_matrix_file = Path(os.environ["PROFILE_MATRIX_FILE"])
soak_file = Path(os.environ["SOAK_FILE"])
conformance_summary_file = Path(os.environ["CONFORMANCE_SUMMARY_FILE"])
conformance_report_md = Path(os.environ["CONFORMANCE_REPORT_MD"])
contract_file = Path(os.environ["CONTRACT_FILE"])


def read_json(path: Path):
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def as_int(value, default=0):
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def as_float(value, default=0.0):
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


summary = read_json(summary_file) or {}
profile_matrix = read_json(profile_matrix_file) or {}
soak_report = read_json(soak_file) or {}
conformance_summary = read_json(conformance_summary_file) or {}
contract_payload = read_json(contract_file)
summary_correlation_id = ""
if isinstance(summary, dict):
    summary_correlation_id = str(summary.get("correlation_id", "")).strip()

counts = conformance_summary.get("counts", {}) if isinstance(conformance_summary, dict) else {}
conformance_total = as_int(counts.get("total"))
conformance_pass = as_int(counts.get("pass"))
conformance_fail = as_int(counts.get("fail"))
conformance_na = as_int(counts.get("na"))
conformance_pass_rate = as_float(conformance_summary.get("pass_rate_pct"))

failed_units = as_int(summary.get("failed_units"))
failed_suites = as_int(summary.get("failed_suites"))
total_units = as_int(summary.get("total_units"))
total_suites = as_int(summary.get("total_suites"))

suite_entries = summary.get("suites", [])
if not isinstance(suite_entries, list):
    suite_entries = []
passing_suites = []
failing_suites = []
for suite in suite_entries:
    if not isinstance(suite, dict):
        continue
    suite_name = str(suite.get("suite", "")).strip()
    if not suite_name:
        continue
    if as_int(suite.get("exit_code"), default=1) == 0:
        passing_suites.append(suite_name)
    else:
        failing_suites.append(suite_name)

unit_entries = summary.get("unit_targets", [])
if not isinstance(unit_entries, list):
    unit_entries = []
failing_unit_targets = []
for target in unit_entries:
    if not isinstance(target, dict):
        continue
    target_name = str(target.get("target", "")).strip()
    if not target_name:
        continue
    if as_int(target.get("exit_code"), default=1) != 0:
        failing_unit_targets.append(target_name)

profiles_payload = profile_matrix.get("profiles", {}) if isinstance(profile_matrix, dict) else {}
if not isinstance(profiles_payload, dict):
    profiles_payload = {}

consistency = (
    profile_matrix.get("preflight_runtime_consistency", {})
    if isinstance(profile_matrix, dict)
    else {}
)
if not isinstance(consistency, dict):
    consistency = {}
mismatches = as_int(consistency.get("mismatched_capability_decisions"))
checked = as_int(consistency.get("checked_capability_decisions"))

profile_order = ["safe", "balanced", "permissive"]
profile_recommendation_defaults = {
    "safe": "Default profile for untrusted extensions",
    "balanced": "Recommended for general extension development",
    "permissive": "Use only for trusted local extensions",
}

profile_rows = {}
for profile_name in profile_order:
    row = profiles_payload.get(profile_name, {})
    if not isinstance(row, dict):
        row = {}
    projected = row.get("projected", {})
    if not isinstance(projected, dict):
        projected = {}
    blocked_summary = row.get("blocked_operation_summary", {})
    if not isinstance(blocked_summary, dict):
        blocked_summary = {}

    blocked = as_int(projected.get("blocked"))
    pass_rate_pct = as_float(projected.get("pass_rate_pct"))
    testable = as_int(projected.get("testable_extensions"))
    projected_pass = as_int(projected.get("pass"))
    projected_fail_existing = as_int(projected.get("fail_existing"))

    if profile_name == "balanced" and pass_rate_pct >= 85.0 and blocked <= 5:
        recommendation = "Recommended default for most users"
    elif profile_name == "safe":
        recommendation = "Safest default with strongest capability restrictions"
    elif profile_name == "permissive":
        recommendation = "Trusted-only mode; enables dangerous capabilities"
    else:
        recommendation = profile_recommendation_defaults[profile_name]

    profile_rows[profile_name] = {
        "projected_pass_rate_pct": round(pass_rate_pct, 2),
        "projected_pass": projected_pass,
        "projected_fail_existing": projected_fail_existing,
        "blocked_extensions": blocked,
        "testable_extensions": testable,
        "recommendation": recommendation,
        "blocked_operation_summary": blocked_summary,
    }

soak_pass = bool(soak_report.get("pass")) if isinstance(soak_report, dict) else False
soak_status = str(soak_report.get("status", "missing")) if isinstance(soak_report, dict) else "missing"
soak_failed_checks = (
    soak_report.get("failed_checks", []) if isinstance(soak_report, dict) else []
)
if not isinstance(soak_failed_checks, list):
    soak_failed_checks = []

overall_ready = (
    failed_units == 0
    and failed_suites == 0
    and conformance_fail == 0
    and conformance_na == 0
    and mismatches == 0
    and soak_pass
)

known_risks = []
remediation_steps = []


def add_risk(risk_id, severity, summary_text, details, evidence, command):
    known_risks.append(
        {
            "id": risk_id,
            "severity": severity,
            "summary": summary_text,
            "details": details,
            "evidence": evidence,
        }
    )
    if command:
        remediation_steps.append(
            {
                "risk_id": risk_id,
                "action": summary_text,
                "command": command,
            }
        )


if failed_units > 0:
    add_risk(
        "unit_target_failures",
        "high",
        "One or more integration/unit targets failed in the verification run",
        f"Failing unit targets: {failing_unit_targets}",
        str(summary_file),
        "./scripts/e2e/run_all.sh --profile focused --skip-lint",
    )

if failed_suites > 0:
    add_risk(
        "e2e_suite_failures",
        "high",
        "One or more user-facing workflow suites failed",
        f"Failing suites: {failing_suites}",
        str(summary_file),
        " ".join(
            [
                "./scripts/e2e/run_all.sh",
                "--profile",
                "focused",
                "--skip-lint",
                *sum((["--suite", suite] for suite in failing_suites), []),
            ]
        ),
    )

if conformance_fail > 0:
    add_risk(
        "conformance_failures",
        "high",
        "Conformance corpus has failing extensions",
        f"{conformance_fail} extension(s) currently fail conformance",
        str(conformance_summary_file),
        "cargo test --test ext_conformance_generated conformance_full_report --features ext-conformance -- --nocapture",
    )

if conformance_na > 0:
    add_risk(
        "conformance_na_gaps",
        "medium",
        "Conformance corpus still includes N/A coverage gaps",
        f"{conformance_na} extension(s) are not yet classified PASS/FAIL",
        str(conformance_summary_file),
        "cargo test --test ext_conformance_scenarios --features ext-conformance scenario_conformance_suite -- --nocapture",
    )

if mismatches > 0:
    add_risk(
        "profile_policy_mismatch",
        "high",
        "Preflight/runtime capability policy mismatch detected",
        f"{mismatches} mismatched capability decision(s) out of {checked} checked",
        str(profile_matrix_file),
        "cargo test --test e2e_cli -- --nocapture",
    )

if not soak_pass:
    add_risk(
        "soak_longevity_failure",
        "high",
        "Soak/longevity verification failed thresholds",
        f"status={soak_status}, failed_checks={soak_failed_checks}",
        str(soak_file),
        "cargo test --test ext_memory_stress ext_memory_stress_inline -- --nocapture && cargo test --test extensions_stress stress_policy_profile_rotation -- --nocapture",
    )

if conformance_report_md.exists():
    report_has_evidence_index = "## Evidence Index" in conformance_report_md.read_text(
        encoding="utf-8"
    )
    if not report_has_evidence_index:
        add_risk(
            "missing_evidence_index",
            "medium",
            "Conformance markdown report missing evidence index section",
            "Expected '## Evidence Index' section not found",
            str(conformance_report_md),
            "cargo test --test conformance_report generate_conformance_report -- --nocapture",
        )

policy_guidance = []
for profile_name in profile_order:
    policy_guidance.append(
        {
            "profile": profile_name,
            "guidance": profile_recommendation_defaults[profile_name],
            "diagnostic_command": (
                f"./target/debug/pi --explain-extension-policy --extension-policy {profile_name}"
            ),
        }
    )

workflow_pass_rate = 100.0
if total_suites > 0:
    workflow_pass_rate = round(((total_suites - failed_suites) / total_suites) * 100.0, 2)

status = "ready" if overall_ready else "not_ready"

payload = {
    "schema": "pi.e2e.release_readiness.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "correlation_id": summary_correlation_id,
    "status": status,
    "overall_ready": overall_ready,
    "summary": {
        "profile": str(summary.get("profile", "unknown")),
        "conformance_total": conformance_total,
        "conformance_pass": conformance_pass,
        "conformance_fail": conformance_fail,
        "conformance_na": conformance_na,
        "conformance_pass_rate_pct": round(conformance_pass_rate, 2),
        "total_units": total_units,
        "failed_units": failed_units,
        "total_suites": total_suites,
        "failed_suites": failed_suites,
    },
    "profiles": profile_rows,
    "workflow_outcomes": {
        "passing_suites": sorted(passing_suites),
        "failing_suites": sorted(failing_suites),
        "failing_unit_targets": sorted(failing_unit_targets),
        "suite_pass_rate_pct": workflow_pass_rate,
    },
    "known_risks": known_risks,
    "remediation": {
        "immediate": remediation_steps,
        "policy_guidance": policy_guidance,
    },
    "evidence": {
        "summary_json": str(summary_file),
        "profile_matrix_json": str(profile_matrix_file),
        "soak_longevity_report_json": str(soak_file),
        "conformance_summary_json": str(conformance_summary_file),
        "conformance_report_markdown": str(conformance_report_md),
        "evidence_contract_json": str(contract_file),
    },
}

readiness_file.parent.mkdir(parents=True, exist_ok=True)
readiness_file.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

lines = [
    "# Release Readiness Summary",
    "",
    f"- Generated at: `{payload['generated_at']}`",
    f"- Status: `{status}`",
    f"- Overall ready: `{overall_ready}`",
    "",
    "## Profile Safety Snapshot",
    "",
    "| Profile | Projected pass rate | Blocked extensions | Recommendation |",
    "|---|---:|---:|---|",
]
for profile_name in profile_order:
    row = profile_rows.get(profile_name, {})
    lines.append(
        f"| {profile_name} | {row.get('projected_pass_rate_pct', 0.0):.2f}% | "
        f"{row.get('blocked_extensions', 0)} | {row.get('recommendation', '')} |"
    )

lines.extend(
    [
        "",
        "## Workflow Outcomes",
        "",
        f"- Suite pass rate: `{workflow_pass_rate:.2f}%`",
        f"- Passing suites ({len(passing_suites)}): "
        + (", ".join(sorted(passing_suites)) if passing_suites else "none"),
        f"- Failing suites ({len(failing_suites)}): "
        + (", ".join(sorted(failing_suites)) if failing_suites else "none"),
        f"- Failing unit targets ({len(failing_unit_targets)}): "
        + (", ".join(sorted(failing_unit_targets)) if failing_unit_targets else "none"),
        "",
        "## Known Risks",
        "",
    ]
)

if known_risks:
    for risk in known_risks:
        lines.extend(
            [
                f"- `{risk['id']}` ({risk['severity']}): {risk['summary']}",
                f"  - details: {risk['details']}",
                f"  - evidence: `{risk['evidence']}`",
            ]
        )
else:
    lines.append("- None")

lines.extend(
    [
        "",
        "## Recommended Remediation",
        "",
    ]
)

if remediation_steps:
    for step in remediation_steps:
        lines.extend(
            [
                f"- `{step['risk_id']}`: {step['action']}",
                f"  - command: `{step['command']}`",
            ]
        )
else:
    lines.append("- No immediate remediation required.")

lines.extend(
    [
        "",
        "## Policy Guidance",
        "",
    ]
)
for guidance in policy_guidance:
    lines.extend(
        [
            f"- `{guidance['profile']}`: {guidance['guidance']}",
            f"  - diagnostic: `{guidance['diagnostic_command']}`",
        ]
    )

readiness_markdown.write_text("\n".join(lines) + "\n", encoding="utf-8")

if isinstance(summary, dict):
    summary["release_readiness"] = {
        "schema": "pi.e2e.release_readiness.v1",
        "correlation_id": summary_correlation_id,
        "path": str(readiness_file),
        "markdown_path": str(readiness_markdown),
        "status": status,
        "overall_ready": overall_ready,
        "risk_count": len(known_risks),
        "failing_suites": sorted(failing_suites),
        "failing_unit_targets": sorted(failing_unit_targets),
    }
    summary_file.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

print("RELEASE READINESS SUMMARY GENERATED")
print(f"- JSON: {readiness_file}")
print(f"- Markdown: {readiness_markdown}")
print(f"- Status: {status}")
print(f"- Risks: {len(known_risks)}")
PY
    then
        echo "[readiness] Release-readiness summary generated ($readiness_file)"
        return 0
    else
        echo "[readiness] Failed to generate release-readiness summary" >&2
        return 1
    fi
}

# ─── Secret Redaction ─────────────────────────────────────────────────────────

redact_secrets() {
    if ARTIFACT_DIR="$ARTIFACT_DIR" python3 - <<'PY'
import os
import re
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
if not artifact_dir.exists():
    raise SystemExit(0)

patterns: list[tuple[re.Pattern[str], str]] = [
    (re.compile(r"\bsk-[A-Za-z0-9_-]{20,}\b"), "sk-REDACTED"),
    (re.compile(r"\bkey-[A-Za-z0-9_-]{20,}\b", re.IGNORECASE), "key-REDACTED"),
    (
        re.compile(r"(?i)\b(Bearer\s+)[A-Za-z0-9._~+/=-]{12,}"),
        r"\1REDACTED",
    ),
    (
        re.compile(
            r"(?i)\b(ANTHROPIC_API_KEY|OPENAI_API_KEY|GOOGLE_API_KEY|AZURE_OPENAI_API_KEY)="
            r"[^\s\"']+"
        ),
        lambda match: f"{match.group(1)}=REDACTED",
    ),
    (
        re.compile(
            r"(?i)(\"?(authorization|x-api-key|api[_-]?key|access[_-]?token|"
            r"refresh[_-]?token|cookie|password|secret)\"?\s*[:=]\s*\"?)([^\s\",]+)(\"?)"
        ),
        r"\1REDACTED\4",
    ),
]

for path in artifact_dir.rglob("*"):
    if not path.is_file():
        continue
    if path.suffix not in {".log", ".jsonl", ".json"}:
        continue
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except Exception:
        continue
    redacted = text
    for pattern, replacement in patterns:
        redacted = pattern.sub(replacement, redacted)
    if redacted != text:
        path.write_text(redacted, encoding="utf-8")
PY
    then
        :
    else
        echo "[redaction] Python redaction pass failed (continuing with unmodified artifacts)" >&2
    fi
}

# ─── Evidence Diff + Triage ──────────────────────────────────────────────────

generate_triage_diff() {
    local baseline_summary="$DIFF_FROM"
    local current_summary="$ARTIFACT_DIR/summary.json"
    local diff_file="$ARTIFACT_DIR/triage_diff.json"

    if [[ -z "$baseline_summary" ]]; then
        return 0
    fi
    if [[ ! -f "$baseline_summary" ]]; then
        echo "[triage] Baseline summary not found: $baseline_summary" >&2
        return 1
    fi
    if [[ ! -f "$current_summary" ]]; then
        echo "[triage] Current summary missing: $current_summary" >&2
        return 1
    fi

    if CURRENT_SUMMARY="$current_summary" \
        BASELINE_SUMMARY="$baseline_summary" \
        DIFF_FILE="$diff_file" \
        PROJECT_ROOT="$PROJECT_ROOT" \
        python3 - <<'PY'
import json
import os
import shlex
import sys
from datetime import datetime, timezone
from pathlib import Path

current_summary_path = Path(os.environ["CURRENT_SUMMARY"])
baseline_summary_path = Path(os.environ["BASELINE_SUMMARY"])
diff_file = Path(os.environ["DIFF_FILE"])
project_root = Path(os.environ["PROJECT_ROOT"])


def read_summary(path: Path) -> dict:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        raise RuntimeError(f"failed to read summary {path}: {exc}") from exc
    if not isinstance(payload, dict):
        raise RuntimeError(f"summary {path} is not a JSON object")
    return payload


def build_index(summary: dict, section_key: str, name_key: str) -> dict[str, dict]:
    indexed: dict[str, dict] = {}
    section = summary.get(section_key, [])
    if not isinstance(section, list):
        return indexed
    for item in section:
        if not isinstance(item, dict):
            continue
        name = item.get(name_key)
        if isinstance(name, str) and name.strip():
            indexed[name] = item
    return indexed


def resolve_path(path_value: object, *, anchor: Path) -> Path | None:
    if not isinstance(path_value, str):
        return None
    candidate_text = path_value.strip()
    if not candidate_text:
        return None
    candidate = Path(candidate_text)
    if not candidate.is_absolute():
        candidate = anchor / candidate
    return candidate.resolve()


def read_json(path: Path) -> dict | None:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None
    if isinstance(payload, dict):
        return payload
    return None


def normalize_scenario_ids(raw_ids: object) -> list[str]:
    if not isinstance(raw_ids, list):
        return []
    scenario_ids = sorted(
        {
            str(item).strip()
            for item in raw_ids
            if isinstance(item, str) and str(item).strip()
        }
    )
    return scenario_ids


def assertion_signature(payload: object) -> str | None:
    if not isinstance(payload, dict):
        return None
    try:
        return json.dumps(payload, sort_keys=True, separators=(",", ":"))
    except Exception:
        return None


def load_failure_diagnostics(summary: dict, summary_path: Path) -> dict[str, dict]:
    diagnostics_meta = summary.get("failure_diagnostics")
    if not isinstance(diagnostics_meta, dict):
        return {}

    index_path = resolve_path(
        diagnostics_meta.get("index_path"),
        anchor=summary_path.parent,
    )
    if index_path is None:
        return {}
    index_payload = read_json(index_path)
    if not isinstance(index_payload, dict):
        return {}

    entries = index_payload.get("suites")
    if not isinstance(entries, list):
        return {}

    diagnostics: dict[str, dict] = {}
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        suite_name = entry.get("suite")
        if not isinstance(suite_name, str) or not suite_name.strip():
            continue
        suite_name = suite_name.strip()

        digest_path = resolve_path(entry.get("digest_path"), anchor=index_path.parent)
        digest_payload = read_json(digest_path) if digest_path is not None else None

        impacted = normalize_scenario_ids(entry.get("impacted_scenario_ids"))
        if not impacted and isinstance(digest_payload, dict):
            impacted = normalize_scenario_ids(digest_payload.get("impacted_scenario_ids"))

        first_assertion = (
            digest_payload.get("first_failing_assertion")
            if isinstance(digest_payload, dict)
            else None
        )
        first_assertion_sig = assertion_signature(first_assertion)

        timeline_event_count = None
        timeline_path = None
        if isinstance(digest_payload, dict):
            timeline_meta = digest_payload.get("timeline")
            if isinstance(timeline_meta, dict):
                event_count = timeline_meta.get("event_count")
                if isinstance(event_count, int):
                    timeline_event_count = event_count
                timeline_path = resolve_path(
                    timeline_meta.get("path"),
                    anchor=digest_path.parent if digest_path is not None else summary_path.parent,
                )

        diagnostics[suite_name] = {
            "suite": suite_name,
            "root_cause_class": str(
                entry.get("root_cause_class")
                or (digest_payload or {}).get("root_cause_class")
                or "unknown"
            ),
            "impacted_scenario_ids": impacted,
            "first_failing_assertion_signature": first_assertion_sig,
            "timeline_event_count": timeline_event_count,
            "digest_path": str(digest_path) if digest_path is not None else None,
            "timeline_path": str(timeline_path) if timeline_path is not None else None,
        }

    return diagnostics


def build_category_map(diff_payload: dict) -> dict[str, str]:
    category_map: dict[str, str] = {}
    category_order = [
        "regressions",
        "new_failures",
        "unresolved_failures",
        "fixed",
        "removed",
        "added_pass",
        "stable_pass",
    ]
    category_name = {
        "regressions": "regression",
        "new_failures": "new_failure",
        "unresolved_failures": "unresolved_failure",
        "fixed": "fixed",
        "removed": "removed",
        "added_pass": "added_pass",
        "stable_pass": "stable_pass",
    }
    for category in category_order:
        records = diff_payload.get(category, [])
        if not isinstance(records, list):
            continue
        for record in records:
            if not isinstance(record, dict):
                continue
            name = record.get("name")
            if isinstance(name, str) and name:
                category_map[name] = category_name.get(category, "unknown")
    return category_map


def build_semantic_diffs(
    suite_categories: dict[str, str],
    baseline_diag: dict[str, dict],
    current_diag: dict[str, dict],
) -> tuple[list[dict], int]:
    all_suites = sorted(set(baseline_diag) | set(current_diag))
    entries: list[dict] = []
    unresolved_count = 0

    for suite_name in all_suites:
        baseline_entry = baseline_diag.get(suite_name)
        current_entry = current_diag.get(suite_name)
        mismatch_kinds: list[str] = []

        if baseline_entry is None and current_entry is not None:
            mismatch_kinds.append("missing_baseline_diagnostics")
        elif baseline_entry is not None and current_entry is None:
            mismatch_kinds.append("missing_current_diagnostics")
        elif baseline_entry is not None and current_entry is not None:
            if baseline_entry.get("root_cause_class") != current_entry.get("root_cause_class"):
                mismatch_kinds.append("root_cause_class_changed")
            if baseline_entry.get("impacted_scenario_ids") != current_entry.get(
                "impacted_scenario_ids"
            ):
                mismatch_kinds.append("impacted_scenarios_changed")
            if baseline_entry.get("first_failing_assertion_signature") != current_entry.get(
                "first_failing_assertion_signature"
            ):
                mismatch_kinds.append("first_failing_assertion_changed")
            if baseline_entry.get("timeline_event_count") != current_entry.get(
                "timeline_event_count"
            ):
                mismatch_kinds.append("timeline_event_count_changed")

        if not mismatch_kinds:
            continue

        category = suite_categories.get(suite_name, "unknown")
        unresolved = category in {"regression", "new_failure", "unresolved_failure"}
        if unresolved:
            unresolved_count += 1

        entries.append(
            {
                "suite": suite_name,
                "classification": category,
                "unresolved": unresolved,
                "mismatch_kinds": mismatch_kinds,
                "artifact_links": {
                    "baseline_digest": None if baseline_entry is None else baseline_entry.get("digest_path"),
                    "current_digest": None if current_entry is None else current_entry.get("digest_path"),
                    "baseline_timeline": None if baseline_entry is None else baseline_entry.get("timeline_path"),
                    "current_timeline": None if current_entry is None else current_entry.get("timeline_path"),
                },
                "baseline": baseline_entry,
                "current": current_entry,
                "recommended_command": f"cargo test --test {shlex.quote(suite_name)} -- --nocapture",
            }
        )

    entries.sort(
        key=lambda entry: (
            0 if entry.get("unresolved") else 1,
            str(entry.get("classification")),
            str(entry.get("suite")),
        )
    )
    return entries, unresolved_count


def build_mirrored_scenario_report(
    *,
    scenario_rows: list[dict],
    baseline_suite_index: dict[str, dict],
    current_suite_index: dict[str, dict],
    semantic_by_suite: dict[str, dict],
) -> dict:
    seen_suites = set(baseline_suite_index) | set(current_suite_index)
    workflows: list[dict] = []
    candidate_workflows = 0

    for row in scenario_rows:
        if not isinstance(row, dict):
            continue
        if row.get("status") != "covered":
            continue

        suite_ids = row.get("suite_ids")
        if not isinstance(suite_ids, list):
            continue
        normalized_suite_ids = [
            suite.strip()
            for suite in suite_ids
            if isinstance(suite, str) and suite.strip()
        ]
        if not normalized_suite_ids:
            continue

        candidate_workflows += 1
        if not any(suite_name in seen_suites for suite_name in normalized_suite_ids):
            continue

        missing_in_baseline = [
            suite_name
            for suite_name in normalized_suite_ids
            if suite_name not in baseline_suite_index
        ]
        missing_in_current = [
            suite_name
            for suite_name in normalized_suite_ids
            if suite_name not in current_suite_index
        ]
        semantic_entries = [
            semantic_by_suite[suite_name]
            for suite_name in normalized_suite_ids
            if suite_name in semantic_by_suite
        ]
        unresolved_semantic_suites = [
            str(entry.get("suite"))
            for entry in semantic_entries
            if bool(entry.get("unresolved"))
        ]

        if missing_in_baseline or missing_in_current:
            status = "incomplete_mirroring"
        elif unresolved_semantic_suites:
            status = "semantic_mismatch"
        else:
            status = "aligned"

        workflows.append(
            {
                "workflow_id": row.get("workflow_id"),
                "workflow_title": row.get("workflow_title"),
                "status": status,
                "suite_ids": normalized_suite_ids,
                "replay_command": row.get("replay_command"),
                "missing_in_baseline": missing_in_baseline,
                "missing_in_current": missing_in_current,
                "unresolved_semantic_suites": unresolved_semantic_suites,
                "artifact_links": {
                    "baseline_summary": str(baseline_summary_path),
                    "current_summary": str(current_summary_path),
                    "baseline_suite_logs": {
                        suite_name: baseline_suite_index[suite_name].get("log_file")
                        for suite_name in normalized_suite_ids
                        if suite_name in baseline_suite_index
                    },
                    "current_suite_logs": {
                        suite_name: current_suite_index[suite_name].get("log_file")
                        for suite_name in normalized_suite_ids
                        if suite_name in current_suite_index
                    },
                },
            }
        )

    workflows.sort(key=lambda entry: str(entry.get("workflow_id") or ""))
    evaluated_workflows = len(workflows)
    incomplete_count = sum(
        1 for entry in workflows if entry.get("status") == "incomplete_mirroring"
    )
    semantic_mismatch_count = sum(
        1 for entry in workflows if entry.get("status") == "semantic_mismatch"
    )
    aligned_count = sum(1 for entry in workflows if entry.get("status") == "aligned")

    return {
        "schema": "pi.e2e.mirrored_scenarios.v1",
        "source_matrix_path": str(project_root / "docs" / "e2e_scenario_matrix.json"),
        "source_sli_contract_path": str(project_root / "docs/perf_sli_matrix.json"),
        "summary": {
            "covered_workflow_candidates": candidate_workflows,
            "evaluated_workflows": evaluated_workflows,
            "aligned_workflows": aligned_count,
            "semantic_mismatch_workflows": semantic_mismatch_count,
            "incomplete_mirroring_workflows": incomplete_count,
        },
        "workflows": workflows,
    }


def classify_change(
    *,
    name: str,
    baseline: dict | None,
    current: dict | None,
) -> tuple[str, dict]:
    baseline_exit = None if baseline is None else baseline.get("exit_code")
    current_exit = None if current is None else current.get("exit_code")

    baseline_ok = baseline_exit == 0
    current_ok = current_exit == 0

    record = {
        "name": name,
        "baseline_exit_code": baseline_exit,
        "current_exit_code": current_exit,
        "baseline_log_file": None if baseline is None else baseline.get("log_file"),
        "current_log_file": None if current is None else current.get("log_file"),
    }

    if baseline is None and current is not None:
        return ("added_pass" if current_ok else "new_failure"), record
    if baseline is not None and current is None:
        return "removed", record
    if baseline_ok and not current_ok:
        return "regression", record
    if not baseline_ok and current_ok:
        return "fixed", record
    if not baseline_ok and not current_ok:
        return "unresolved_failure", record
    return "stable_pass", record


def diff_section(
    *,
    baseline: dict,
    current: dict,
    section_key: str,
    name_key: str,
) -> dict:
    baseline_index = build_index(baseline, section_key, name_key)
    current_index = build_index(current, section_key, name_key)

    categories = {
        "regressions": [],
        "fixed": [],
        "unresolved_failures": [],
        "new_failures": [],
        "added_pass": [],
        "removed": [],
        "stable_pass": [],
    }

    all_names = sorted(set(baseline_index) | set(current_index))
    for name in all_names:
        category, record = classify_change(
            name=name,
            baseline=baseline_index.get(name),
            current=current_index.get(name),
        )
        if category == "new_failure":
            categories["new_failures"].append(record)
        else:
            categories[category if category in categories else "stable_pass"].append(record)

    return {
        "baseline_total": len(baseline_index),
        "current_total": len(current_index),
        **categories,
    }


def build_runner_command(unit_targets: list[str], suites: list[str]) -> str:
    parts = ["./scripts/e2e/run_all.sh", "--profile", "focused", "--skip-lint"]
    if unit_targets:
        for target in unit_targets:
            parts.extend(["--unit-target", target])
    else:
        parts.append("--skip-unit")
    for suite in suites:
        parts.extend(["--suite", suite])
    return " ".join(shlex.quote(part) for part in parts)


def collect_names(records: list[dict]) -> list[str]:
    return [str(record["name"]) for record in records if isinstance(record.get("name"), str)]


def build_target_commands(unit_names: list[str], suite_names: list[str]) -> dict:
    commands: dict[str, str] = {}
    for name in unit_names:
        commands[f"unit:{name}"] = f"cargo test --test {shlex.quote(name)} -- --nocapture"
    for name in suite_names:
        commands[f"suite:{name}"] = f"cargo test --test {shlex.quote(name)} -- --nocapture"
    return commands


def build_ranked_diagnostics(unit_diff: dict, suite_diff: dict) -> list[dict]:
    category_priority = {
        "regressions": 100,
        "new_failures": 90,
        "unresolved_failures": 70,
        "fixed": 40,
        "removed": 30,
        "added_pass": 20,
        "stable_pass": 10,
    }
    category_label = {
        "regressions": "regression",
        "new_failures": "new_failure",
        "unresolved_failures": "unresolved_failure",
        "fixed": "fixed",
        "removed": "removed",
        "added_pass": "added_pass",
        "stable_pass": "stable_pass",
    }
    category_order = [
        "regressions",
        "new_failures",
        "unresolved_failures",
        "fixed",
        "removed",
        "added_pass",
        "stable_pass",
    ]

    diagnostics: list[dict] = []
    for surface, section in (("unit", unit_diff), ("suite", suite_diff)):
        for category in category_order:
            for record in section.get(category, []):
                name = record.get("name")
                if not isinstance(name, str) or not name:
                    continue
                diagnostics.append(
                    {
                        "surface": surface,
                        "name": name,
                        "severity": category_label[category],
                        "priority_score": category_priority[category],
                        "baseline_exit_code": record.get("baseline_exit_code"),
                        "current_exit_code": record.get("current_exit_code"),
                        "baseline_log_file": record.get("baseline_log_file"),
                        "current_log_file": record.get("current_log_file"),
                        "recommended_command": (
                            f"cargo test --test {shlex.quote(name)} -- --nocapture"
                        ),
                    }
                )

    diagnostics.sort(
        key=lambda item: (
            -int(item["priority_score"]),
            str(item["surface"]),
            str(item["name"]),
        )
    )
    for index, item in enumerate(diagnostics, start=1):
        item["rank"] = index

    return diagnostics


try:
    baseline_summary = read_summary(baseline_summary_path)
    current_summary = read_summary(current_summary_path)
except RuntimeError as exc:
    print(f"[triage] {exc}", file=sys.stderr)
    sys.exit(1)

unit_diff = diff_section(
    baseline=baseline_summary,
    current=current_summary,
    section_key="unit_targets",
    name_key="target",
)
suite_diff = diff_section(
    baseline=baseline_summary,
    current=current_summary,
    section_key="suites",
    name_key="suite",
)

unit_regressions = collect_names(unit_diff["regressions"])
unit_new_failures = collect_names(unit_diff["new_failures"])
unit_unresolved = collect_names(unit_diff["unresolved_failures"])
suite_regressions = collect_names(suite_diff["regressions"])
suite_new_failures = collect_names(suite_diff["new_failures"])
suite_unresolved = collect_names(suite_diff["unresolved_failures"])

triage_unit_focus = sorted(set(unit_regressions + unit_new_failures + unit_unresolved))
triage_suite_focus = sorted(set(suite_regressions + suite_new_failures + suite_unresolved))

regression_count = len(unit_regressions) + len(suite_regressions)
new_failure_count = len(unit_new_failures) + len(suite_new_failures)
unresolved_count = len(unit_unresolved) + len(suite_unresolved)
fixed_count = len(unit_diff["fixed"]) + len(suite_diff["fixed"])
suite_categories = build_category_map(suite_diff)
baseline_suite_index = build_index(baseline_summary, "suites", "suite")
current_suite_index = build_index(current_summary, "suites", "suite")
baseline_diag = load_failure_diagnostics(baseline_summary, baseline_summary_path)
current_diag = load_failure_diagnostics(current_summary, current_summary_path)
semantic_diffs, unresolved_semantic_count = build_semantic_diffs(
    suite_categories=suite_categories,
    baseline_diag=baseline_diag,
    current_diag=current_diag,
)
semantic_by_suite = {
    str(entry["suite"]): entry
    for entry in semantic_diffs
    if isinstance(entry, dict) and isinstance(entry.get("suite"), str)
}

scenario_matrix_path = project_root / "docs" / "e2e_scenario_matrix.json"
scenario_rows: list[dict] = []
scenario_matrix_payload = read_json(scenario_matrix_path)
if isinstance(scenario_matrix_payload, dict):
    rows = scenario_matrix_payload.get("rows")
    if isinstance(rows, list):
        scenario_rows = [row for row in rows if isinstance(row, dict)]

mirrored_scenarios = build_mirrored_scenario_report(
    scenario_rows=scenario_rows,
    baseline_suite_index=baseline_suite_index,
    current_suite_index=current_suite_index,
    semantic_by_suite=semantic_by_suite,
)
mirrored_summary = (
    mirrored_scenarios.get("summary", {})
    if isinstance(mirrored_scenarios, dict)
    else {}
)
mirrored_semantic_mismatch_workflow_count = (
    mirrored_summary.get("semantic_mismatch_workflows", 0)
    if isinstance(mirrored_summary.get("semantic_mismatch_workflows", 0), int)
    else 0
)
mirrored_incomplete_workflow_count = (
    mirrored_summary.get("incomplete_mirroring_workflows", 0)
    if isinstance(mirrored_summary.get("incomplete_mirroring_workflows", 0), int)
    else 0
)
mirrored_evaluated_workflow_count = (
    mirrored_summary.get("evaluated_workflows", 0)
    if isinstance(mirrored_summary.get("evaluated_workflows", 0), int)
    else 0
)

recommended_commands = {
    "runner_repro_command": build_runner_command(triage_unit_focus, triage_suite_focus)
    if triage_unit_focus or triage_suite_focus
    else "",
    "target_commands": build_target_commands(triage_unit_focus, triage_suite_focus),
}
ranked_diagnostics = build_ranked_diagnostics(unit_diff, suite_diff)
ranked_repro_commands: list[str] = []
seen_commands: set[str] = set()
for item in ranked_diagnostics:
    severity = item.get("severity")
    command = item.get("recommended_command")
    if severity not in {"regression", "new_failure", "unresolved_failure"}:
        continue
    if not isinstance(command, str) or not command:
        continue
    if command in seen_commands:
        continue
    seen_commands.add(command)
    ranked_repro_commands.append(command)
recommended_commands["ranked_repro_commands"] = ranked_repro_commands
semantic_focus_commands: list[str] = []
seen_semantic_commands: set[str] = set()
for entry in semantic_diffs:
    if not isinstance(entry, dict):
        continue
    if not bool(entry.get("unresolved")):
        continue
    command = entry.get("recommended_command")
    if not isinstance(command, str) or not command:
        continue
    if command in seen_semantic_commands:
        continue
    seen_semantic_commands.add(command)
    semantic_focus_commands.append(command)
recommended_commands["semantic_focus_commands"] = semantic_focus_commands

status = "regression" if (regression_count > 0 or new_failure_count > 0) else "stable"
if status == "stable" and unresolved_count > 0:
    status = "known_failures_only"
if status in {"stable", "known_failures_only"} and unresolved_semantic_count > 0:
    status = "semantic_mismatch"
if status == "stable" and mirrored_incomplete_workflow_count > 0:
    status = "incomplete_mirroring"

payload = {
    "schema": "pi.e2e.triage_diff.v1",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "status": status,
    "baseline_summary_path": str(baseline_summary_path),
    "current_summary_path": str(current_summary_path),
    "baseline_artifact_dir": baseline_summary.get("artifact_dir"),
    "current_artifact_dir": current_summary.get("artifact_dir"),
    "summary": {
        "regression_count": regression_count,
        "new_failure_count": new_failure_count,
        "unresolved_failure_count": unresolved_count,
        "fixed_count": fixed_count,
        "unit_regression_count": len(unit_regressions),
        "suite_regression_count": len(suite_regressions),
        "semantic_mismatch_count": unresolved_semantic_count,
        "mirrored_evaluated_workflow_count": mirrored_evaluated_workflow_count,
        "mirrored_semantic_mismatch_workflow_count": mirrored_semantic_mismatch_workflow_count,
        "mirrored_incomplete_workflow_count": mirrored_incomplete_workflow_count,
    },
    "unit_targets": unit_diff,
    "suites": suite_diff,
    "focus": {
        "unit_targets": triage_unit_focus,
        "suites": triage_suite_focus,
    },
    "semantic_diffs": {
        "schema": "pi.e2e.semantic_diff.v1",
        "unresolved_count": unresolved_semantic_count,
        "entries": semantic_diffs,
    },
    "mirrored_scenarios": mirrored_scenarios,
    "ranked_diagnostics": ranked_diagnostics,
    "recommended_commands": recommended_commands,
}

diff_file.parent.mkdir(parents=True, exist_ok=True)
diff_file.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

current_summary["triage_diff"] = {
    "schema": "pi.e2e.triage_diff.v1",
    "path": str(diff_file),
    "baseline_summary_path": str(baseline_summary_path),
    "status": status,
    "regression_count": regression_count,
    "new_failure_count": new_failure_count,
    "unresolved_failure_count": unresolved_count,
    "fixed_count": fixed_count,
    "semantic_mismatch_count": unresolved_semantic_count,
    "mirrored_evaluated_workflow_count": mirrored_evaluated_workflow_count,
    "mirrored_semantic_mismatch_workflow_count": mirrored_semantic_mismatch_workflow_count,
    "mirrored_incomplete_workflow_count": mirrored_incomplete_workflow_count,
    "top_ranked_diagnostics": ranked_diagnostics[:5],
    "top_semantic_diffs": semantic_diffs[:5],
}
current_summary_path.write_text(json.dumps(current_summary, indent=2) + "\n", encoding="utf-8")

print("TRIAGE DIFF GENERATED")
print(f"- Baseline: {baseline_summary_path}")
print(f"- Current:  {current_summary_path}")
print(
    "- Summary: regressions="
    f"{regression_count}, new_failures={new_failure_count}, unresolved={unresolved_count}, fixed={fixed_count}"
)
print(
    "- Semantic: mismatches="
    f"{unresolved_semantic_count}, mirrored_workflows={mirrored_evaluated_workflow_count}, "
    f"mirrored_semantic_mismatches={mirrored_semantic_mismatch_workflow_count}, "
    f"mirrored_incomplete={mirrored_incomplete_workflow_count}"
)
if recommended_commands["runner_repro_command"]:
    print(f"- Repro runner: {recommended_commands['runner_repro_command']}")
if recommended_commands["semantic_focus_commands"]:
    print("- Semantic repro focus:")
    for command in recommended_commands["semantic_focus_commands"][:5]:
        print(f"  - {command}")
if ranked_diagnostics:
    print("- Ranked diagnostics:")
    for item in ranked_diagnostics[:5]:
        print(
            f"  {item['rank']}. [{item['severity']}] "
            f"{item['surface']}:{item['name']} -> {item['recommended_command']}"
        )
print(f"- Diff artifact: {diff_file}")
sys.exit(0)
PY
    then
        echo "[triage] Diff report written to $diff_file"
        return 0
    else
        echo "[triage] Diff generation failed" >&2
        return 1
    fi
}

# ─── Evidence Contract Validation ────────────────────────────────────────────

validate_evidence_contract() {
    local contract_file="$ARTIFACT_DIR/evidence_contract.json"
    local runner_outcome_file="$ARTIFACT_DIR/runner_outcome.json"
    local selected_units_json selected_suites_json
    local all_unit_targets_json all_suites_json
    local perf_baseline_confidence_json perf_extension_stratification_json
    local claim_integrity_required_json franken_node_claim_tier_json

    selected_units_json="$(
        printf '%s\n' "${SELECTED_UNIT_TARGETS[@]:-}" | \
            python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'
    )"
    selected_suites_json="$(
        printf '%s\n' "${SELECTED_SUITES[@]:-}" | \
            python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'
    )"
    all_unit_targets_json="$(
        printf '%s\n' "${ALL_UNIT_TARGETS[@]:-}" | \
            python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'
    )"
    all_suites_json="$(
        printf '%s\n' "${ALL_SUITES[@]:-}" | \
            python3 -c 'import json,sys; print(json.dumps([l.strip() for l in sys.stdin if l.strip()]))' 2>/dev/null || echo '[]'
    )"
    perf_baseline_confidence_json="${PERF_BASELINE_CONFIDENCE_JSON:-}"
    perf_extension_stratification_json="${PERF_EXTENSION_STRATIFICATION_JSON:-}"
    perf_phase1_matrix_validation_json="${PERF_PHASE1_MATRIX_VALIDATION_JSON:-}"
    claim_integrity_required_json="${CLAIM_INTEGRITY_REQUIRED:-}"
    franken_node_claim_tier_json="${FRANKEN_NODE_CLAIM_TIER:-}"

    if ARTIFACT_DIR="$ARTIFACT_DIR" \
        PROJECT_ROOT="$PROJECT_ROOT" \
        GENERATED_AT="$GENERATED_AT" \
        SOURCE_COMMIT="$SOURCE_COMMIT" \
        SOURCE_SNAPSHOT="$SOURCE_SNAPSHOT" \
        VERIFY_PROFILE_NAME="$PROFILE" \
        CONTRACT_FILE="$contract_file" \
        RUNNER_OUTCOME_FILE="$runner_outcome_file" \
        SELECTED_UNITS_JSON="$selected_units_json" \
        SELECTED_SUITES_JSON="$selected_suites_json" \
        ALL_UNIT_TARGETS_JSON="$all_unit_targets_json" \
        ALL_SUITES_JSON="$all_suites_json" \
        RERUN_FROM_JSON="$RERUN_JSON_VALUE" \
        PERF_BASELINE_CONFIDENCE_JSON="$perf_baseline_confidence_json" \
        PERF_EXTENSION_STRATIFICATION_JSON="$perf_extension_stratification_json" \
        PERF_PHASE1_MATRIX_VALIDATION_JSON="$perf_phase1_matrix_validation_json" \
        PERF_EVIDENCE_DIR="${PERF_EVIDENCE_DIR:-}" \
        CLAIM_INTEGRITY_REQUIRED="$claim_integrity_required_json" \
        FRANKEN_NODE_CLAIM_TIER="$franken_node_claim_tier_json" \
        CI_ENV="${CI:-}" \
        python3 - <<'PY'
import hashlib
import json
import math
import os
import re
import stat
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

artifact_dir = Path(os.environ["ARTIFACT_DIR"])
project_root = Path(os.environ["PROJECT_ROOT"])
generated_at = os.environ["GENERATED_AT"]
source_commit = os.environ["SOURCE_COMMIT"]
source_snapshot = os.environ["SOURCE_SNAPSHOT"]
profile = os.environ.get("VERIFY_PROFILE_NAME", "full")
contract_file = Path(os.environ["CONTRACT_FILE"])
runner_outcome_path = Path(os.environ["RUNNER_OUTCOME_FILE"])

try:
    selected_units = json.loads(os.environ.get("SELECTED_UNITS_JSON", "[]"))
except json.JSONDecodeError:
    selected_units = []
try:
    selected_suites = json.loads(os.environ.get("SELECTED_SUITES_JSON", "[]"))
except json.JSONDecodeError:
    selected_suites = []
try:
    all_unit_targets = json.loads(os.environ.get("ALL_UNIT_TARGETS_JSON", "[]"))
except json.JSONDecodeError:
    all_unit_targets = []
try:
    all_suites = json.loads(os.environ.get("ALL_SUITES_JSON", "[]"))
except json.JSONDecodeError:
    all_suites = []
try:
    rerun_from = json.loads(os.environ.get("RERUN_FROM_JSON", "null"))
except json.JSONDecodeError:
    rerun_from = None


def env_truthy(name: str, default: bool = False) -> bool:
    raw = os.environ.get(name)
    if raw is None:
        return default
    normalized = raw.strip().lower()
    if normalized in {"1", "true", "yes", "y", "on"}:
        return True
    if normalized in {"0", "false", "no", "n", "off"}:
        return False
    return default


perf_evidence_dir_raw = str(os.environ.get("PERF_EVIDENCE_DIR", "")).strip()
perf_baseline_confidence_path_raw = str(
    os.environ.get("PERF_BASELINE_CONFIDENCE_JSON", "")
).strip()
perf_extension_stratification_path_raw = str(
    os.environ.get("PERF_EXTENSION_STRATIFICATION_JSON", "")
).strip()
perf_phase1_matrix_validation_path_raw = str(
    os.environ.get("PERF_PHASE1_MATRIX_VALIDATION_JSON", "")
).strip()
perf_parameter_sweeps_path_raw = str(
    os.environ.get("PERF_PARAMETER_SWEEPS_JSON", "")
).strip()

if perf_evidence_dir_raw:
    if not perf_baseline_confidence_path_raw:
        perf_baseline_confidence_path_raw = str(
            Path(perf_evidence_dir_raw) / "results" / "baseline_variance_confidence.json"
        )
    if not perf_extension_stratification_path_raw:
        perf_extension_stratification_path_raw = str(
            Path(perf_evidence_dir_raw) / "results" / "extension_benchmark_stratification.json"
        )
    if not perf_phase1_matrix_validation_path_raw:
        perf_phase1_matrix_validation_path_raw = str(
            Path(perf_evidence_dir_raw) / "results" / "phase1_matrix_validation.json"
        )
    if not perf_parameter_sweeps_path_raw:
        perf_parameter_sweeps_path_raw = str(
            Path(perf_evidence_dir_raw) / "results" / "parameter_sweeps.json"
        )

perf_baseline_confidence_path = (
    Path(perf_baseline_confidence_path_raw) if perf_baseline_confidence_path_raw else None
)
perf_extension_stratification_path = (
    Path(perf_extension_stratification_path_raw) if perf_extension_stratification_path_raw else None
)
perf_phase1_matrix_validation_path = (
    Path(perf_phase1_matrix_validation_path_raw)
    if perf_phase1_matrix_validation_path_raw
    else None
)
perf_parameter_sweeps_path = (
    Path(perf_parameter_sweeps_path_raw) if perf_parameter_sweeps_path_raw else None
)
ci_env = env_truthy("CI_ENV", default=env_truthy("CI", default=False))
claim_integrity_required = env_truthy(
    "CLAIM_INTEGRITY_REQUIRED",
    default=ci_env and profile in {"ci", "full"},
)
requested_claim_tier_raw = str(os.environ.get("FRANKEN_NODE_CLAIM_TIER", "")).strip()

checks = []
errors = []
warnings = []
remediation_hints: set[str] = set()


def add_check(check_id: str, path: Path, ok: bool, diagnostics: str) -> None:
    checks.append(
        {
            "id": check_id,
            "path": str(path),
            "ok": ok,
            "diagnostics": diagnostics,
        }
    )


def record_issue(check_id: str, message: str, *, strict: bool, remediation: str | None = None) -> None:
    rendered = f"{check_id}: {message}"
    if remediation:
        remediation_hints.add(remediation)
        rendered = f"{rendered} | remediation: {remediation}"
    if strict:
        errors.append(rendered)
    else:
        warnings.append(rendered)


def remediation_for_missing_keys(check_id: str, path: Path, missing: list[str]) -> str:
    missing_list = ", ".join(missing)
    if check_id.startswith("environment"):
        return f"Update capture_env() to emit key(s): {missing_list}."
    if check_id.startswith("summary"):
        return f"Update write_summary() to emit key(s): {missing_list}."
    if check_id.startswith("failure_diagnostics"):
        return (
            "Update generate_failure_diagnostics() payloads in scripts/e2e/run_all.sh "
            f"to include key(s): {missing_list}."
        )
    if check_id.startswith(("lib:", "unit:", "suite:")):
        return (
            "Update the lib/unit/suite result.json emitters to include "
            f"key(s): {missing_list}."
        )
    if check_id.startswith("conformance.release_readiness"):
        return (
            "Update generate_release_readiness_report() payload in scripts/e2e/run_all.sh "
            f"to include key(s): {missing_list}."
        )
    if check_id.startswith("conformance.profile_matrix"):
        return (
            "Update generate_extension_profile_matrix() output to include "
            f"key(s): {missing_list}."
        )
    return (
        "Update scripts/e2e/run_all.sh artifact generation so "
        f"{path.name} includes key(s): {missing_list}."
    )


def require_file(
    check_id: str,
    path: Path,
    *,
    strict: bool,
    description: str,
    remediation: str | None = None,
) -> bool:
    ok = path.exists()
    add_check(check_id, path, ok, description if ok else f"missing required file: {path}")
    if not ok:
        record_issue(
            check_id,
            f"missing {path}",
            strict=strict,
            remediation=(
                remediation
                or f"Ensure the producer step writes {path.name} before validation."
            ),
        )
    return ok


def load_json(check_id: str, path: Path, *, strict: bool) -> dict | None:
    if not require_file(check_id, path, strict=strict, description="file exists"):
        return None
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:  # pragma: no cover - defensive
        add_check(check_id + ".json_parse", path, False, f"invalid JSON: {exc}")
        record_issue(
            check_id,
            f"invalid JSON ({exc})",
            strict=strict,
            remediation=f"Rewrite {path.name} as valid JSON and rerun ./scripts/e2e/run_all.sh.",
        )
        return None
    add_check(check_id + ".json_parse", path, True, "valid JSON")
    return payload


def require_keys(
    check_id: str,
    payload: dict | None,
    path: Path,
    keys: list[str],
    *,
    strict: bool,
) -> None:
    if payload is None:
        return
    missing = [key for key in keys if key not in payload]
    ok = not missing
    add_check(
        check_id + ".keys",
        path,
        ok,
        "all required keys present" if ok else f"missing keys: {', '.join(missing)}",
    )
    if not ok:
        record_issue(
            check_id,
            f"missing keys in {path}: {', '.join(missing)}",
            strict=strict,
            remediation=remediation_for_missing_keys(check_id, path, missing),
        )


def require_condition(
    check_id: str,
    *,
    path: Path,
    ok: bool,
    ok_msg: str,
    fail_msg: str,
    strict: bool,
    remediation: str | None = None,
) -> None:
    add_check(check_id, path, ok, ok_msg if ok else fail_msg)
    if ok:
        return
    record_issue(check_id, fail_msg, strict=strict, remediation=remediation)


# 1) Core run artifacts (always required)
environment_path = artifact_dir / "environment.json"
environment = load_json("environment", environment_path, strict=True)
require_keys(
    "environment",
    environment,
    environment_path,
    [
        "schema",
        "generated_at",
        "timestamp",
        "profile",
        "rerun_from",
        "diff_from",
        "rustc",
        "cargo",
        "os",
        "git_sha",
        "git_branch",
        "source_commit",
        "source_snapshot",
        "parallelism",
        "log_level",
        "artifact_dir",
        "correlation_id",
        "shard",
        "unit_targets",
        "e2e_suites",
    ],
    strict=True,
)

summary_path = artifact_dir / "summary.json"
summary = load_json("summary", summary_path, strict=True)
require_keys(
    "summary",
    summary,
    summary_path,
    [
        "schema",
        "generated_at",
        "timestamp",
        "profile",
        "rerun_from",
        "diff_from",
        "artifact_dir",
        "correlation_id",
        "source_commit",
        "source_snapshot",
        "shard",
        "total_units",
        "passed_units",
        "failed_units",
        "total_suites",
        "passed_suites",
        "failed_suites",
        "lib",
        "runner_outcome",
        "unit_targets",
        "suites",
        "failure_diagnostics",
    ],
    strict=True,
)

runner_outcome = load_json("runner_outcome", runner_outcome_path, strict=True)
require_keys(
    "runner_outcome",
    runner_outcome,
    runner_outcome_path,
    [
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
    ],
    strict=True,
)

environment_correlation_id = ""
if isinstance(environment, dict):
    require_condition(
        "environment.schema",
        path=environment_path,
        ok=environment.get("schema") == "pi.e2e.environment.v1",
        ok_msg="environment schema matches",
        fail_msg=f"expected schema 'pi.e2e.environment.v1', got {environment.get('schema')!r}",
        strict=True,
        remediation="Update capture_env() schema to pi.e2e.environment.v1.",
    )
    environment_correlation_id = str(environment.get("correlation_id", "")).strip()
    require_condition(
        "environment.correlation_id_nonempty",
        path=environment_path,
        ok=bool(environment_correlation_id),
        ok_msg="environment correlation_id is set",
        fail_msg="environment correlation_id is empty",
        strict=True,
        remediation="Set CORRELATION_ID before capture_env() and emit it in environment.json.",
    )

summary_correlation_id = ""
if isinstance(summary, dict):
    require_condition(
        "summary.schema",
        path=summary_path,
        ok=summary.get("schema") == "pi.e2e.summary.v1",
        ok_msg="summary schema matches",
        fail_msg=f"expected schema 'pi.e2e.summary.v1', got {summary.get('schema')!r}",
        strict=True,
        remediation="Update write_summary() schema to pi.e2e.summary.v1.",
    )
    summary_correlation_id = str(summary.get("correlation_id", "")).strip()
    require_condition(
        "summary.correlation_id_nonempty",
        path=summary_path,
        ok=bool(summary_correlation_id),
        ok_msg="summary correlation_id is set",
        fail_msg="summary correlation_id is empty",
        strict=True,
        remediation="Emit CORRELATION_ID in write_summary() output.",
    )

if summary_correlation_id and environment_correlation_id:
    require_condition(
        "run.correlation_id_matches_environment",
        path=summary_path,
        ok=summary_correlation_id == environment_correlation_id,
        ok_msg="summary/environment correlation_id values match",
        fail_msg=(
            "summary/environment correlation_id mismatch: "
            f"{summary_correlation_id!r} vs {environment_correlation_id!r}"
        ),
        strict=True,
        remediation=(
            "Propagate the same CORRELATION_ID through capture_env() and write_summary()."
        ),
    )

if isinstance(environment, dict):
    require_condition(
        "environment.generated_at_matches_run",
        path=environment_path,
        ok=environment.get("generated_at") == generated_at,
        ok_msg="environment generated_at matches the run timestamp",
        fail_msg=(
            "environment generated_at mismatch: expected "
            f"{generated_at!r}, got {environment.get('generated_at')!r}"
        ),
        strict=True,
        remediation="Emit the run-level GENERATED_AT value in environment.json.",
    )

if isinstance(summary, dict):
    require_condition(
        "summary.generated_at_matches_run",
        path=summary_path,
        ok=summary.get("generated_at") == generated_at,
        ok_msg="summary generated_at matches the run timestamp",
        fail_msg=(
            "summary generated_at mismatch: expected "
            f"{generated_at!r}, got {summary.get('generated_at')!r}"
        ),
        strict=True,
        remediation="Emit the run-level GENERATED_AT value in summary.json.",
    )

if isinstance(environment, dict) and isinstance(summary, dict):
    require_condition(
        "run.generated_at_matches_environment",
        path=summary_path,
        ok=summary.get("generated_at") == environment.get("generated_at"),
        ok_msg="summary/environment generated_at values match",
        fail_msg=(
            "summary/environment generated_at mismatch: "
            f"{summary.get('generated_at')!r} vs {environment.get('generated_at')!r}"
        ),
        strict=True,
        remediation=(
            "Propagate the same run-level GENERATED_AT value through capture_env() "
            "and write_summary()."
        ),
    )

source_commit_ok = bool(
    re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", source_commit)
)
source_snapshot_ok = bool(re.fullmatch(r"sha256:[0-9a-f]{64}", source_snapshot))
require_condition(
    "run.source_commit_format",
    path=contract_file,
    ok=source_commit_ok,
    ok_msg="captured source commit is a canonical full object ID",
    fail_msg=f"captured source commit is malformed: {source_commit!r}",
    strict=True,
    remediation="Rerun from a canonical Git checkout with a valid HEAD commit.",
)
require_condition(
    "run.source_snapshot_format",
    path=contract_file,
    ok=source_snapshot_ok,
    ok_msg="captured source snapshot uses canonical sha256 identity syntax",
    fail_msg=f"captured source snapshot is malformed: {source_snapshot!r}",
    strict=True,
    remediation="Rerun so the producer can capture a clean immutable source snapshot.",
)

environment_source_commit = (
    environment.get("source_commit") if isinstance(environment, dict) else None
)
environment_source_snapshot = (
    environment.get("source_snapshot") if isinstance(environment, dict) else None
)
summary_source_commit = summary.get("source_commit") if isinstance(summary, dict) else None
summary_source_snapshot = (
    summary.get("source_snapshot") if isinstance(summary, dict) else None
)

if isinstance(environment, dict):
    require_condition(
        "environment.source_commit_format",
        path=environment_path,
        ok=isinstance(environment_source_commit, str)
        and bool(re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", environment_source_commit)),
        ok_msg="environment source_commit is a canonical full object ID",
        fail_msg=f"environment source_commit is malformed: {environment_source_commit!r}",
        strict=True,
        remediation="Emit SOURCE_COMMIT from capture_env().",
    )
    require_condition(
        "environment.source_snapshot_format",
        path=environment_path,
        ok=isinstance(environment_source_snapshot, str)
        and bool(re.fullmatch(r"sha256:[0-9a-f]{64}", environment_source_snapshot)),
        ok_msg="environment source_snapshot uses canonical sha256 identity syntax",
        fail_msg=f"environment source_snapshot is malformed: {environment_source_snapshot!r}",
        strict=True,
        remediation="Emit SOURCE_SNAPSHOT from capture_env().",
    )
    require_condition(
        "environment.source_commit_matches_run",
        path=environment_path,
        ok=source_commit_ok and environment_source_commit == source_commit,
        ok_msg="environment source_commit matches the captured run source",
        fail_msg=(
            "environment source_commit does not match the captured run source: "
            f"{environment_source_commit!r} vs {source_commit!r}"
        ),
        strict=True,
        remediation="Propagate the captured SOURCE_COMMIT unchanged into environment.json.",
    )
    require_condition(
        "environment.source_snapshot_matches_run",
        path=environment_path,
        ok=source_snapshot_ok and environment_source_snapshot == source_snapshot,
        ok_msg="environment source_snapshot matches the captured run source",
        fail_msg=(
            "environment source_snapshot does not match the captured run source: "
            f"{environment_source_snapshot!r} vs {source_snapshot!r}"
        ),
        strict=True,
        remediation="Propagate the captured SOURCE_SNAPSHOT unchanged into environment.json.",
    )
    require_condition(
        "environment.git_sha_matches_source_commit",
        path=environment_path,
        ok=source_commit_ok and environment.get("git_sha") == source_commit,
        ok_msg="environment git_sha matches the captured source commit",
        fail_msg=(
            "environment git_sha does not match the captured source commit: "
            f"{environment.get('git_sha')!r} vs {source_commit!r}"
        ),
        strict=True,
        remediation="Populate environment git_sha from SOURCE_COMMIT.",
    )

if isinstance(summary, dict):
    require_condition(
        "summary.source_commit_format",
        path=summary_path,
        ok=isinstance(summary_source_commit, str)
        and bool(re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", summary_source_commit)),
        ok_msg="summary source_commit is a canonical full object ID",
        fail_msg=f"summary source_commit is malformed: {summary_source_commit!r}",
        strict=True,
        remediation="Emit SOURCE_COMMIT from write_summary().",
    )
    require_condition(
        "summary.source_snapshot_format",
        path=summary_path,
        ok=isinstance(summary_source_snapshot, str)
        and bool(re.fullmatch(r"sha256:[0-9a-f]{64}", summary_source_snapshot)),
        ok_msg="summary source_snapshot uses canonical sha256 identity syntax",
        fail_msg=f"summary source_snapshot is malformed: {summary_source_snapshot!r}",
        strict=True,
        remediation="Emit SOURCE_SNAPSHOT from write_summary().",
    )
    require_condition(
        "summary.source_commit_matches_run",
        path=summary_path,
        ok=source_commit_ok and summary_source_commit == source_commit,
        ok_msg="summary source_commit matches the captured run source",
        fail_msg=(
            "summary source_commit does not match the captured run source: "
            f"{summary_source_commit!r} vs {source_commit!r}"
        ),
        strict=True,
        remediation="Propagate the captured SOURCE_COMMIT unchanged into summary.json.",
    )
    require_condition(
        "summary.source_snapshot_matches_run",
        path=summary_path,
        ok=source_snapshot_ok and summary_source_snapshot == source_snapshot,
        ok_msg="summary source_snapshot matches the captured run source",
        fail_msg=(
            "summary source_snapshot does not match the captured run source: "
            f"{summary_source_snapshot!r} vs {source_snapshot!r}"
        ),
        strict=True,
        remediation="Propagate the captured SOURCE_SNAPSHOT unchanged into summary.json.",
    )

if isinstance(environment, dict) and isinstance(summary, dict):
    require_condition(
        "run.source_commit_matches_environment",
        path=summary_path,
        ok=summary_source_commit == environment_source_commit,
        ok_msg="summary/environment source_commit values match",
        fail_msg=(
            "summary/environment source_commit mismatch: "
            f"{summary_source_commit!r} vs {environment_source_commit!r}"
        ),
        strict=True,
        remediation="Propagate one captured SOURCE_COMMIT through all run artifacts.",
    )
    require_condition(
        "run.source_snapshot_matches_environment",
        path=summary_path,
        ok=summary_source_snapshot == environment_source_snapshot,
        ok_msg="summary/environment source_snapshot values match",
        fail_msg=(
            "summary/environment source_snapshot mismatch: "
            f"{summary_source_snapshot!r} vs {environment_source_snapshot!r}"
        ),
        strict=True,
        remediation="Propagate one captured SOURCE_SNAPSHOT through all run artifacts.",
    )

if isinstance(runner_outcome, dict):
    expected_runner_outcome_keys = {
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
    require_condition(
        "runner_outcome.keys_exact",
        path=runner_outcome_path,
        ok=set(runner_outcome) == expected_runner_outcome_keys,
        ok_msg="runner outcome contains exactly the versioned schema fields",
        fail_msg="runner outcome contains missing or unsupported fields",
        strict=True,
        remediation="Regenerate runner_outcome.json with write_runner_outcome().",
    )
    require_condition(
        "runner_outcome.schema",
        path=runner_outcome_path,
        ok=runner_outcome.get("schema") == "pi.e2e.runner_outcome.v1",
        ok_msg="runner outcome schema matches",
        fail_msg=(
            "expected schema 'pi.e2e.runner_outcome.v1', got "
            f"{runner_outcome.get('schema')!r}"
        ),
        strict=True,
        remediation="Emit schema pi.e2e.runner_outcome.v1.",
    )
    require_condition(
        "runner_outcome.generated_at_matches_run",
        path=runner_outcome_path,
        ok=runner_outcome.get("generated_at") == generated_at,
        ok_msg="runner outcome generated_at matches the run",
        fail_msg="runner outcome generated_at does not match the run",
        strict=True,
    )
    require_condition(
        "runner_outcome.timestamp_matches_run",
        path=runner_outcome_path,
        ok=runner_outcome.get("timestamp") == artifact_dir.name,
        ok_msg="runner outcome timestamp matches the artifact directory",
        fail_msg="runner outcome timestamp does not match the artifact directory",
        strict=True,
    )
    require_condition(
        "runner_outcome.profile_matches_run",
        path=runner_outcome_path,
        ok=runner_outcome.get("profile") == profile,
        ok_msg="runner outcome profile matches the run",
        fail_msg="runner outcome profile does not match the run",
        strict=True,
    )
    require_condition(
        "runner_outcome.artifact_dir_matches_run",
        path=runner_outcome_path,
        ok=runner_outcome.get("artifact_dir") == str(artifact_dir),
        ok_msg="runner outcome artifact_dir matches the run",
        fail_msg="runner outcome artifact_dir does not match the run",
        strict=True,
    )
    require_condition(
        "runner_outcome.correlation_id_matches_run",
        path=runner_outcome_path,
        ok=bool(summary_correlation_id)
        and runner_outcome.get("correlation_id") == summary_correlation_id,
        ok_msg="runner outcome correlation_id matches the run",
        fail_msg="runner outcome correlation_id does not match the run",
        strict=True,
    )
    require_condition(
        "runner_outcome.source_commit_matches_run",
        path=runner_outcome_path,
        ok=source_commit_ok and runner_outcome.get("source_commit") == source_commit,
        ok_msg="runner outcome source_commit matches the captured run source",
        fail_msg="runner outcome source_commit does not match the captured run source",
        strict=True,
    )
    require_condition(
        "runner_outcome.source_snapshot_matches_run",
        path=runner_outcome_path,
        ok=source_snapshot_ok and runner_outcome.get("source_snapshot") == source_snapshot,
        ok_msg="runner outcome source_snapshot matches the captured run source",
        fail_msg="runner outcome source_snapshot does not match the captured run source",
        strict=True,
    )
    require_condition(
        "runner_outcome.status_pass",
        path=runner_outcome_path,
        ok=runner_outcome.get("status") == "pass",
        ok_msg="runner outcome status is pass",
        fail_msg=f"runner outcome status is {runner_outcome.get('status')!r}",
        strict=True,
        remediation="Resolve every failed producer phase and rerun the complete profile.",
    )
    runner_exit_code = runner_outcome.get("exit_code")
    require_condition(
        "runner_outcome.exit_code_zero",
        path=runner_outcome_path,
        ok=not isinstance(runner_exit_code, bool)
        and isinstance(runner_exit_code, int)
        and runner_exit_code == 0,
        ok_msg="runner outcome exit_code is zero",
        fail_msg=f"runner outcome exit_code is {runner_exit_code!r}",
        strict=True,
        remediation="Resolve every failed producer phase and rerun the complete profile.",
    )
    require_condition(
        "runner_outcome.source_snapshot_verified",
        path=runner_outcome_path,
        ok=runner_outcome.get("source_snapshot_verified") is True,
        ok_msg="runner completed a matching final source snapshot verification",
        fail_msg="runner did not complete a matching final source snapshot verification",
        strict=True,
        remediation="Rerun from an immutable clean source checkout.",
    )
    require_condition(
        "runner_outcome.failed_phases_empty",
        path=runner_outcome_path,
        ok=runner_outcome.get("failed_phases") == [],
        ok_msg="runner outcome contains no failed phases",
        fail_msg=f"runner failed phases: {runner_outcome.get('failed_phases')!r}",
        strict=True,
        remediation="Resolve every listed producer phase and rerun the complete profile.",
    )

if isinstance(summary, dict):
    require_condition(
        "summary.runner_outcome_matches_file",
        path=summary_path,
        ok=isinstance(runner_outcome, dict)
        and summary.get("runner_outcome") == runner_outcome,
        ok_msg="summary embeds the exact runner outcome",
        fail_msg="summary runner_outcome does not match runner_outcome.json",
        strict=True,
        remediation="Write runner_outcome.json and its summary embedding in one producer step.",
    )

# Full-profile, unsharded baseline runs must cover every configured target and
# suite. Partial shards remain useful CI evidence, but must never claim strict
# conformance (the release gate rejects full-profile evidence with that flag
# disabled).
summary_shard = summary.get("shard") if isinstance(summary, dict) else None
shard_kind = summary_shard.get("kind") if isinstance(summary_shard, dict) else None
is_sharded = shard_kind in {"unit", "suite", "both"}
strict_conformance = profile == "full" and rerun_from is None and not is_sharded
full_scope_path = artifact_dir / "summary.json"
if profile == "full":
    require_condition(
        "full_profile.rerun_mode",
        path=full_scope_path,
        ok=rerun_from is None,
        ok_msg="full profile baseline run (not rerun)",
        fail_msg="full profile rerun detected; scope checks downgraded to warnings",
        strict=False,
    )
    require_condition(
        "full_profile.shard_mode",
        path=full_scope_path,
        ok=not is_sharded,
        ok_msg="full profile baseline run is unsharded",
        fail_msg="full profile shard detected; scope checks downgraded to warnings",
        strict=False,
    )

    missing_units = sorted(set(all_unit_targets) - set(selected_units))
    extra_units = sorted(set(selected_units) - set(all_unit_targets))
    require_condition(
        "full_profile.unit_scope_complete",
        path=full_scope_path,
        ok=not missing_units and not extra_units,
        ok_msg=f"selected all unit targets ({len(all_unit_targets)})",
        fail_msg=(
            f"unit scope mismatch; missing={missing_units if missing_units else []}, "
            f"extra={extra_units if extra_units else []}"
        ),
        strict=strict_conformance,
    )

    missing_suites = sorted(set(all_suites) - set(selected_suites))
    extra_suites = sorted(set(selected_suites) - set(all_suites))
    require_condition(
        "full_profile.e2e_scope_complete",
        path=full_scope_path,
        ok=not missing_suites and not extra_suites,
        ok_msg=f"selected all e2e suites ({len(all_suites)})",
        fail_msg=(
            f"e2e scope mismatch; missing={missing_suites if missing_suites else []}, "
            f"extra={extra_suites if extra_suites else []}"
        ),
        strict=strict_conformance,
    )

LOG_REQUIRED_FIELDS = {
    "pi.test.log.v1": [
        "schema",
        "type",
        "seq",
        "ts",
        "t_ms",
        "level",
        "category",
        "message",
    ],
    "pi.test.log.v2": [
        "schema",
        "type",
        "trace_id",
        "seq",
        "ts",
        "t_ms",
        "level",
        "category",
        "message",
    ],
}
ARTIFACT_REQUIRED_FIELDS = ["schema", "type", "seq", "ts", "t_ms", "name", "path"]
MAX_OUTPUT_LOG_BYTES = 8 * 1024 * 1024
MAX_TEST_LOG_JSONL_BYTES = 4 * 1024 * 1024
MAX_ARTIFACT_INDEX_JSONL_BYTES = 4 * 1024 * 1024
MAX_JSONL_RECORDS = 25_000

LEAK_PATTERNS: list[tuple[str, re.Pattern[str]]] = [
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
]


def normalized_jsonl_path(path: Path) -> Path:
    file_name = path.name
    if file_name.endswith(".jsonl"):
        base = file_name[: -len(".jsonl")]
    else:
        base = file_name
    return path.with_name(f"{base}.normalized.jsonl")


def find_sensitive_leaks(text: str, *, limit: int = 3) -> list[str]:
    hits: list[str] = []
    for label, pattern in LEAK_PATTERNS:
        for match in pattern.finditer(text):
            snippet = match.group(0)
            upper = snippet.upper()
            if "REDACTED" in upper or "<TRACE_ID>" in snippet or "<TIMESTAMP>" in snippet:
                continue
            hits.append(f"{label}:{snippet[:96]}")
            if len(hits) >= limit:
                return hits
    return hits


def validate_text_redaction(
    *,
    check_id: str,
    path: Path,
    strict: bool,
) -> None:
    if not path.exists():
        return
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except Exception as exc:  # pragma: no cover - defensive
        add_check(check_id, path, False, f"redaction scan failed to read file: {exc}")
        record_issue(
            check_id,
            f"unable to scan file for sensitive leaks ({exc})",
            strict=strict,
            remediation=f"Ensure {path.name} is readable before evidence validation.",
        )
        return
    leaks = find_sensitive_leaks(text)
    require_condition(
        check_id,
        path=path,
        ok=not leaks,
        ok_msg="no high-confidence secret/token patterns detected",
        fail_msg=f"potential sensitive leaks detected: {leaks}",
        strict=strict,
        remediation=(
            "Redact authorization/api-key/token values in artifact output and rerun verification."
        ),
    )


def validate_file_budget(
    *,
    check_id: str,
    path: Path,
    max_bytes: int,
    strict: bool,
    remediation: str,
) -> None:
    if not path.exists():
        return
    size_bytes = path.stat().st_size
    require_condition(
        check_id,
        path=path,
        ok=size_bytes <= max_bytes,
        ok_msg=f"file size {size_bytes} bytes within budget <= {max_bytes}",
        fail_msg=f"file size {size_bytes} bytes exceeds budget {max_bytes}",
        strict=strict,
        remediation=remediation,
    )

validate_text_redaction(
    check_id="environment.redaction_scan",
    path=environment_path,
    strict=True,
)
validate_text_redaction(
    check_id="summary.redaction_scan",
    path=summary_path,
    strict=True,
)


def path_matches(value: object, expected: Path) -> bool:
    text = str(value or "").strip()
    if not text:
        return False
    candidate = Path(text)
    if candidate == expected:
        return True
    try:
        return candidate.resolve() == expected.resolve()
    except Exception:  # pragma: no cover - defensive
        return False


def parse_iso8601_timestamp(value: object) -> datetime | None:
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = f"{text[:-1]}+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def read_jsonl_lines(check_id: str, path: Path, *, strict: bool) -> list[tuple[int, str]]:
    if not require_file(
        check_id,
        path,
        strict=strict,
        description="jsonl file exists",
        remediation=f"Ensure test harness writes {path.name} for every selected suite/target.",
    ):
        return []
    try:
        lines = [line for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
    except Exception as exc:  # pragma: no cover - defensive
        add_check(f"{check_id}.readable", path, False, f"read failed: {exc}")
        record_issue(
            check_id,
            f"failed reading {path}: {exc}",
            strict=strict,
            remediation=f"Fix file permissions/content for {path} and rerun verification.",
        )
        return []
    add_check(f"{check_id}.readable", path, True, f"loaded {len(lines)} non-empty JSONL lines")
    return list(enumerate(lines, start=1))


def validate_jsonl_record_keys(
    *,
    check_id: str,
    path: Path,
    record: dict,
    required_keys: list[str],
    line_no: int,
    strict: bool,
    remediation: str,
) -> None:
    missing = [key for key in required_keys if key not in record]
    require_condition(
        f"{check_id}.line_{line_no}.required_keys",
        path=path,
        ok=not missing,
        ok_msg="all required keys present",
        fail_msg=f"line {line_no} missing keys: {missing}",
        strict=strict,
        remediation=remediation,
    )


def parse_test_log_jsonl(check_prefix: str, path: Path) -> dict[str, set[str]]:
    trace_ids_by_test: dict[str, set[str]] = {}
    observed_categories: set[str] = set()
    lines = read_jsonl_lines(f"{check_prefix}.test_log_jsonl", path, strict=True)
    require_condition(
        f"{check_prefix}.test_log_jsonl.record_budget",
        path=path,
        ok=len(lines) <= MAX_JSONL_RECORDS,
        ok_msg=f"test-log record count {len(lines)} within budget <= {MAX_JSONL_RECORDS}",
        fail_msg=(
            f"test-log record count {len(lines)} exceeds budget {MAX_JSONL_RECORDS}"
        ),
        strict=True,
        remediation="Reduce noisy log emission or scope logging in tests/common/logging.",
    )
    require_condition(
        f"{check_prefix}.test_log_jsonl.non_empty",
        path=path,
        ok=len(lines) > 0,
        ok_msg=f"test-log contains {len(lines)} records",
        fail_msg="test-log.jsonl is empty",
        strict=True,
        remediation="Ensure tests/common/logging writes at least one log record per target/suite.",
    )
    for line_no, line in lines:
        record_id = f"{check_prefix}.test_log_jsonl.line_{line_no}"
        try:
            payload = json.loads(line)
        except Exception as exc:  # pragma: no cover - defensive
            add_check(record_id, path, False, f"invalid JSON: {exc}")
            record_issue(
                record_id,
                f"invalid JSON: {exc}",
                strict=True,
                remediation="Emit valid JSONL records in test-log.jsonl.",
            )
            continue
        if not isinstance(payload, dict):
            add_check(record_id, path, False, "record must be a JSON object")
            record_issue(
                record_id,
                "record must be a JSON object",
                strict=True,
                remediation="Write object records (not arrays/scalars) to test-log.jsonl.",
            )
            continue
        schema = payload.get("schema")
        schema_text = str(schema)
        required = LOG_REQUIRED_FIELDS.get(schema_text)
        is_artifact_record = schema_text == "pi.test.artifact.v1"
        require_condition(
            f"{record_id}.schema_supported",
            path=path,
            ok=required is not None or is_artifact_record,
            ok_msg=f"supported schema {schema!r}",
            fail_msg=f"unsupported test-log schema {schema!r}",
            strict=True,
            remediation=(
                "Use pi.test.log.v2 (or v1 where legacy is explicitly required) "
                "or pi.test.artifact.v1 for inline artifact records."
            ),
        )
        if is_artifact_record:
            validate_jsonl_record_keys(
                check_id=record_id,
                path=path,
                record=payload,
                required_keys=ARTIFACT_REQUIRED_FIELDS,
                line_no=line_no,
                strict=True,
                remediation=(
                    "Ensure inline artifact records in test-log.jsonl emit required "
                    "pi.test.artifact.v1 fields."
                ),
            )
            continue
        if required is None:
            continue
        validate_jsonl_record_keys(
            check_id=record_id,
            path=path,
            record=payload,
            required_keys=required,
            line_no=line_no,
            strict=True,
            remediation="Ensure tests/common/logging emits required test-log schema fields.",
        )
        category = payload.get("category")
        if isinstance(category, str) and category.strip():
            observed_categories.add(category.strip())
        test_name = payload.get("test")
        if isinstance(test_name, str) and test_name.strip():
            trace_id = payload.get("trace_id")
            if isinstance(trace_id, str) and trace_id.strip():
                trace_ids_by_test.setdefault(test_name, set()).add(trace_id)
        else:
            require_condition(
                f"{record_id}.test_field_present",
                path=path,
                ok=False,
                ok_msg="test field present",
                fail_msg=f"line {line_no} missing non-empty test field",
                strict=strict_conformance,
                remediation="Populate the `test` field in every test-log JSONL record.",
            )
    multi_trace_tests = sorted(
        [name for name, trace_ids in trace_ids_by_test.items() if len(trace_ids) > 1]
    )
    require_condition(
        f"{check_prefix}.test_log_jsonl.trace_ids_consistent",
        path=path,
        ok=not multi_trace_tests,
        ok_msg="trace_id values are consistent per test",
        fail_msg=f"multiple trace_id values detected for tests: {multi_trace_tests}",
        strict=strict_conformance,
        remediation="Use a stable trace_id per test invocation.",
    )
    require_condition(
        f"{check_prefix}.test_log_jsonl.minimum_signal_harness_category",
        path=path,
        ok="harness" in observed_categories,
        ok_msg="test-log includes harness category signal",
        fail_msg=(
            f"test-log missing harness category signal; observed categories={sorted(observed_categories)}"
        ),
        strict=True,
        remediation=(
            "Ensure tests/common/harness emits start/completion harness log entries to preserve minimum triage signal."
        ),
    )
    return trace_ids_by_test


def parse_artifact_index_jsonl(
    check_prefix: str,
    path: Path,
    trace_ids_by_test: dict[str, set[str]],
) -> None:
    lines = read_jsonl_lines(f"{check_prefix}.artifact_index_jsonl", path, strict=True)
    if not lines:
        add_check(
            f"{check_prefix}.artifact_index_jsonl.empty_ok",
            path,
            True,
            "artifact-index has no records (allowed when no artifacts were emitted)",
        )
        return
    for line_no, line in lines:
        record_id = f"{check_prefix}.artifact_index_jsonl.line_{line_no}"
        try:
            payload = json.loads(line)
        except Exception as exc:  # pragma: no cover - defensive
            add_check(record_id, path, False, f"invalid JSON: {exc}")
            record_issue(
                record_id,
                f"invalid JSON: {exc}",
                strict=True,
                remediation="Emit valid JSONL records in artifact-index.jsonl.",
            )
            continue
        if not isinstance(payload, dict):
            add_check(record_id, path, False, "record must be a JSON object")
            record_issue(
                record_id,
                "record must be a JSON object",
                strict=True,
                remediation="Write object records (not arrays/scalars) to artifact-index.jsonl.",
            )
            continue
        require_condition(
            f"{record_id}.schema",
            path=path,
            ok=payload.get("schema") == "pi.test.artifact.v1",
            ok_msg="artifact-index schema matches",
            fail_msg=f"unexpected artifact schema {payload.get('schema')!r}",
            strict=True,
            remediation="Emit artifact-index records with schema pi.test.artifact.v1.",
        )
        validate_jsonl_record_keys(
            check_id=record_id,
            path=path,
            record=payload,
            required_keys=ARTIFACT_REQUIRED_FIELDS,
            line_no=line_no,
            strict=True,
            remediation="Ensure tests/common/logging emits required artifact-index schema fields.",
        )
        test_name = payload.get("test")
        has_test = isinstance(test_name, str) and bool(test_name.strip())
        require_condition(
            f"{record_id}.test_field_present",
            path=path,
            ok=has_test,
            ok_msg="artifact record includes test name",
            fail_msg=f"line {line_no} missing non-empty test field",
            strict=strict_conformance,
            remediation="Emit `test` in artifact-index records to support trace linkage.",
        )
        if has_test and strict_conformance:
            linked_trace_ids = trace_ids_by_test.get(str(test_name), set())
            require_condition(
                f"{record_id}.trace_linked_to_test_log",
                path=path,
                ok=bool(linked_trace_ids),
                ok_msg=f"artifact test {test_name!r} links to trace_id set {sorted(linked_trace_ids)}",
                fail_msg=(
                    f"artifact test {test_name!r} has no matching test-log trace_id context"
                ),
                strict=True,
                remediation=(
                    "Ensure artifact-index `test` names match test-log records emitted by the same suite/target."
                ),
            )


def validate_normalized_jsonl_pair(
    *,
    check_prefix: str,
    raw_path: Path,
    normalized_path: Path,
    allowed_schemas: set[str],
    enforce_path_placeholder: bool,
) -> None:
    raw_lines = read_jsonl_lines(f"{check_prefix}.raw", raw_path, strict=True)
    normalized_lines = read_jsonl_lines(f"{check_prefix}.normalized", normalized_path, strict=True)

    require_condition(
        f"{check_prefix}.line_count_matches_raw",
        path=normalized_path,
        ok=len(raw_lines) == len(normalized_lines),
        ok_msg="normalized JSONL line count matches raw JSONL",
        fail_msg=(
            f"normalized/raw line-count mismatch: normalized={len(normalized_lines)} raw={len(raw_lines)}"
        ),
        strict=True,
        remediation=(
            "Emit normalized JSONL as a 1:1 transformation of raw JSONL records "
            "to preserve retention/index completeness."
        ),
    )
    require_condition(
        f"{check_prefix}.record_budget",
        path=normalized_path,
        ok=len(normalized_lines) <= MAX_JSONL_RECORDS,
        ok_msg=f"normalized JSONL record count {len(normalized_lines)} within budget <= {MAX_JSONL_RECORDS}",
        fail_msg=(
            f"normalized JSONL record count {len(normalized_lines)} exceeds budget {MAX_JSONL_RECORDS}"
        ),
        strict=True,
        remediation="Reduce noisy log/artifact emission or partition runs.",
    )

    for line_no, line in normalized_lines:
        record_id = f"{check_prefix}.line_{line_no}"
        try:
            payload = json.loads(line)
        except Exception as exc:  # pragma: no cover - defensive
            add_check(record_id, normalized_path, False, f"invalid JSON: {exc}")
            record_issue(
                record_id,
                f"invalid JSON: {exc}",
                strict=True,
                remediation=(
                    "Write valid JSON objects to normalized JSONL output files."
                ),
            )
            continue
        if not isinstance(payload, dict):
            add_check(record_id, normalized_path, False, "record must be a JSON object")
            record_issue(
                record_id,
                "record must be a JSON object",
                strict=True,
                remediation="Emit object records in normalized JSONL output files.",
            )
            continue

        schema = str(payload.get("schema", ""))
        require_condition(
            f"{record_id}.schema_allowed",
            path=normalized_path,
            ok=schema in allowed_schemas,
            ok_msg=f"schema {schema!r} allowed",
            fail_msg=f"schema {schema!r} not allowed in normalized JSONL",
            strict=True,
            remediation=(
                "Keep normalized JSONL schema set aligned to test-log/artifact schemas."
            ),
        )
        require_condition(
            f"{record_id}.timestamp_normalized",
            path=normalized_path,
            ok=str(payload.get("ts", "")) == "<TIMESTAMP>",
            ok_msg="normalized timestamp placeholder is applied",
            fail_msg=f"normalized ts must be <TIMESTAMP> (got {payload.get('ts')!r})",
            strict=True,
            remediation="Normalize timestamps to <TIMESTAMP> in normalized JSONL writers.",
        )
        require_condition(
            f"{record_id}.elapsed_normalized",
            path=normalized_path,
            ok=payload.get("t_ms") == 0,
            ok_msg="normalized elapsed t_ms is 0",
            fail_msg=f"normalized t_ms must be 0 (got {payload.get('t_ms')!r})",
            strict=True,
            remediation="Normalize elapsed timing fields to deterministic placeholder values.",
        )

        trace_id = payload.get("trace_id")
        if trace_id is not None:
            require_condition(
                f"{record_id}.trace_id_normalized",
                path=normalized_path,
                ok=str(trace_id) == "<TRACE_ID>",
                ok_msg="trace_id normalized to <TRACE_ID>",
                fail_msg=f"trace_id not normalized (got {trace_id!r})",
                strict=True,
                remediation="Normalize trace_id values to <TRACE_ID> in normalized log output.",
            )

        for field in ("span_id", "parent_span_id"):
            if payload.get(field) is not None:
                require_condition(
                    f"{record_id}.{field}_normalized",
                    path=normalized_path,
                    ok=str(payload.get(field)) == "<SPAN_ID>",
                    ok_msg=f"{field} normalized to <SPAN_ID>",
                    fail_msg=f"{field} not normalized (got {payload.get(field)!r})",
                    strict=True,
                    remediation=f"Normalize {field} values to <SPAN_ID> in normalized log output.",
                )

        if enforce_path_placeholder and "path" in payload:
            normalized_path_value = str(payload.get("path", ""))
            has_placeholder = (
                "<TEST_ROOT>" in normalized_path_value
                or "<PROJECT_ROOT>" in normalized_path_value
            )
            require_condition(
                f"{record_id}.path_placeholder",
                path=normalized_path,
                ok=has_placeholder or not normalized_path_value.startswith("/"),
                ok_msg="artifact path uses deterministic placeholders",
                fail_msg=(
                    f"normalized path should use placeholders (got {normalized_path_value!r})"
                ),
                strict=True,
                remediation=(
                    "Normalize artifact paths to <TEST_ROOT> / <PROJECT_ROOT> placeholders."
                ),
            )

    validate_text_redaction(
        check_id=f"{check_prefix}.redaction_scan",
        path=normalized_path,
        strict=True,
    )


def validate_result_contract(
    *,
    kind: str,
    name: str,
    result_path: Path,
    expected_log_path: Path,
    expected_test_log_path: Path | None,
    expected_artifact_index_path: Path | None,
) -> dict | None:
    check_prefix = f"{kind}:{name}"
    result = load_json(f"{check_prefix}:result", result_path, strict=True)
    key_name = "target" if kind in {"lib", "unit"} else "suite"
    required_result_keys = [
        "schema",
        "result_kind",
        "correlation_id",
        key_name,
        "exit_code",
        "duration_ms",
        "passed",
        "failed",
        "ignored",
        "total",
        "log_file",
        "diagnostic_artifacts",
        "timestamp",
    ]
    if expected_test_log_path is not None:
        required_result_keys.append("test_log_jsonl")
    if expected_artifact_index_path is not None:
        required_result_keys.append("artifact_index_jsonl")
    require_keys(
        f"{check_prefix}:result",
        result,
        result_path,
        required_result_keys,
        strict=True,
    )
    if not isinstance(result, dict):
        return None

    require_condition(
        f"{check_prefix}:result.schema",
        path=result_path,
        ok=result.get("schema") == "pi.e2e.result.v1",
        ok_msg="result schema matches",
        fail_msg=f"expected schema 'pi.e2e.result.v1', got {result.get('schema')!r}",
        strict=True,
        remediation="Emit schema pi.e2e.result.v1 in result.json.",
    )
    require_condition(
        f"{check_prefix}:result.kind",
        path=result_path,
        ok=result.get("result_kind") == kind,
        ok_msg=f"result_kind is {kind!r}",
        fail_msg=f"expected result_kind {kind!r}, got {result.get('result_kind')!r}",
        strict=True,
        remediation="Set result_kind to lib/unit/suite in the corresponding result emitter.",
    )
    require_condition(
        f"{check_prefix}:result.name",
        path=result_path,
        ok=str(result.get(key_name, "")) == name,
        ok_msg=f"{key_name} matches selected item",
        fail_msg=(
            f"{key_name} mismatch: expected {name!r}, got {result.get(key_name)!r}"
        ),
        strict=True,
        remediation="Emit the exact suite/target identifier in result.json.",
    )
    require_condition(
        f"{check_prefix}:result.correlation_id_matches_summary",
        path=result_path,
        ok=bool(summary_correlation_id)
        and str(result.get("correlation_id", "")).strip() == summary_correlation_id,
        ok_msg="result correlation_id matches summary correlation_id",
        fail_msg=(
            "result correlation_id mismatch: "
            f"expected {summary_correlation_id!r}, got {result.get('correlation_id')!r}"
        ),
        strict=True,
        remediation="Propagate CORRELATION_ID into every result.json emitter.",
    )
    exit_code = result.get("exit_code")
    require_condition(
        f"{check_prefix}:result.exit_code_zero",
        path=result_path,
        ok=not isinstance(exit_code, bool)
        and isinstance(exit_code, int)
        and exit_code == 0,
        ok_msg="result exit_code is zero",
        fail_msg=f"result exit_code is {exit_code!r}",
        strict=True,
        remediation=f"Resolve the failing {kind} test and rerun the complete profile.",
    )
    duration_ms = result.get("duration_ms")
    require_condition(
        f"{check_prefix}:result.duration_ms_nonnegative",
        path=result_path,
        ok=not isinstance(duration_ms, bool)
        and isinstance(duration_ms, int)
        and duration_ms >= 0,
        ok_msg="result duration_ms is a non-negative integer",
        fail_msg=f"result duration_ms is invalid: {duration_ms!r}",
        strict=True,
    )
    counts = [result.get(field) for field in ("passed", "failed", "ignored", "total")]
    counts_are_uints = all(
        not isinstance(value, bool) and isinstance(value, int) and value >= 0
        for value in counts
    )
    require_condition(
        f"{check_prefix}:result.counts_nonnegative",
        path=result_path,
        ok=counts_are_uints,
        ok_msg="result counts are non-negative integers",
        fail_msg=f"result counts are invalid: {counts!r}",
        strict=True,
    )
    passed, failed, ignored, total = counts
    counts_consistent = (
        counts_are_uints
        and passed + failed + ignored == total
        and failed == 0
    )
    require_condition(
        f"{check_prefix}:result.counts_consistent",
        path=result_path,
        ok=counts_consistent,
        ok_msg="result counts are internally consistent and contain no failures",
        fail_msg=f"result counts are inconsistent or failing: {counts!r}",
        strict=True,
        remediation=f"Resolve the failing {kind} test and regenerate result.json.",
    )
    require_condition(
        f"{check_prefix}:result.tests_executed",
        path=result_path,
        ok=counts_are_uints and total > 0 and passed > 0,
        ok_msg="result proves at least one passing test executed",
        fail_msg=f"result does not prove any passing tests executed: {counts!r}",
        strict=True,
        remediation=f"Ensure the {kind} selector executes at least one real test.",
    )
    require_condition(
        f"{check_prefix}:result.timestamp_matches_run",
        path=result_path,
        ok=result.get("timestamp") == artifact_dir.name,
        ok_msg="result timestamp matches the run",
        fail_msg="result timestamp does not match the run",
        strict=True,
    )

    diagnostic_artifacts = result.get("diagnostic_artifacts")
    require_condition(
        f"{check_prefix}:result.diagnostic_artifacts.object",
        path=result_path,
        ok=isinstance(diagnostic_artifacts, dict),
        ok_msg="diagnostic_artifacts is an object",
        fail_msg="diagnostic_artifacts is missing or not an object",
        strict=True,
        remediation="Emit byte bindings for every retained diagnostic artifact.",
    )
    if isinstance(diagnostic_artifacts, dict):
        require_condition(
            f"{check_prefix}:result.diagnostic_artifacts.keys_exact",
            path=result_path,
            ok=set(diagnostic_artifacts)
            == {
                "schema",
                "output_log",
                "test_log_jsonl",
                "artifact_index_jsonl",
            },
            ok_msg="diagnostic_artifacts contains exactly the versioned schema fields",
            fail_msg="diagnostic_artifacts contains missing or unsupported fields",
            strict=True,
            remediation="Emit only the versioned diagnostic artifact binding fields.",
        )
        require_condition(
            f"{check_prefix}:result.diagnostic_artifacts.schema",
            path=result_path,
            ok=diagnostic_artifacts.get("schema") == "pi.e2e.diagnostic_artifacts.v1",
            ok_msg="diagnostic_artifacts schema matches",
            fail_msg=(
                "expected diagnostic_artifacts schema "
                f"'pi.e2e.diagnostic_artifacts.v1', got {diagnostic_artifacts.get('schema')!r}"
            ),
            strict=True,
            remediation="Emit schema pi.e2e.diagnostic_artifacts.v1.",
        )

    def validate_diagnostic_binding(field: str, expected_path: Path) -> None:
        binding = (
            diagnostic_artifacts.get(field)
            if isinstance(diagnostic_artifacts, dict)
            else None
        )
        binding_prefix = f"{check_prefix}:result.diagnostic_artifacts.{field}"
        require_condition(
            f"{binding_prefix}.object",
            path=result_path,
            ok=isinstance(binding, dict),
            ok_msg=f"{field} binding is an object",
            fail_msg=f"{field} binding is missing or not an object",
            strict=True,
            remediation=f"Emit path, sha256, and size_bytes for {field}.",
        )
        if not isinstance(binding, dict):
            return
        require_keys(
            binding_prefix,
            binding,
            result_path,
            ["path", "sha256", "size_bytes"],
            strict=True,
        )

        bound_path = binding.get("path")
        bound_sha256 = binding.get("sha256")
        bound_size = binding.get("size_bytes")
        require_condition(
            f"{binding_prefix}.path_matches",
            path=result_path,
            ok=isinstance(bound_path, str) and path_matches(bound_path, expected_path),
            ok_msg=f"{field} binding path matches the retained artifact",
            fail_msg=(
                f"{field} binding path mismatch; expected {expected_path}, got {bound_path!r}"
            ),
            strict=True,
            remediation=f"Bind {field} to its exact retained artifact path.",
        )
        require_condition(
            f"{binding_prefix}.sha256_format",
            path=result_path,
            ok=isinstance(bound_sha256, str)
            and bool(re.fullmatch(r"sha256:[0-9a-f]{64}", bound_sha256)),
            ok_msg=f"{field} binding sha256 uses canonical syntax",
            fail_msg=f"{field} binding sha256 is malformed: {bound_sha256!r}",
            strict=True,
            remediation=f"Hash the finalized raw bytes of {field} with SHA-256.",
        )
        require_condition(
            f"{binding_prefix}.size_format",
            path=result_path,
            ok=not isinstance(bound_size, bool)
            and isinstance(bound_size, int)
            and 0 <= bound_size <= 2**63 - 1,
            ok_msg=f"{field} binding size_bytes is a non-negative integer",
            fail_msg=f"{field} binding size_bytes is invalid: {bound_size!r}",
            strict=True,
            remediation=f"Emit the finalized raw byte length for {field}.",
        )

        try:
            before = expected_path.lstat()
        except OSError as exc:
            require_condition(
                f"{binding_prefix}.regular_non_executable",
                path=expected_path,
                ok=False,
                ok_msg=f"{field} is a regular non-executable file",
                fail_msg=f"cannot inspect retained {field}: {exc}",
                strict=True,
                remediation=f"Retain {field} as a regular non-executable file.",
            )
            return
        regular_non_executable = stat.S_ISREG(before.st_mode) and not (
            before.st_mode & 0o111
        )
        require_condition(
            f"{binding_prefix}.regular_non_executable",
            path=expected_path,
            ok=regular_non_executable,
            ok_msg=f"{field} is a regular non-executable file",
            fail_msg=f"{field} is a symlink, special file, or executable",
            strict=True,
            remediation=f"Retain {field} as a regular non-executable file.",
        )
        if not regular_non_executable:
            return
        try:
            raw_bytes = expected_path.read_bytes()
            after = expected_path.lstat()
        except OSError as exc:
            require_condition(
                f"{binding_prefix}.stable_read",
                path=expected_path,
                ok=False,
                ok_msg=f"{field} remained stable while read",
                fail_msg=f"cannot read retained {field} stably: {exc}",
                strict=True,
                remediation=f"Stop concurrent writers to {field} and rerun verification.",
            )
            return
        stable_fields = ("st_dev", "st_ino", "st_mode", "st_size", "st_mtime_ns", "st_ctime_ns")
        stable_read = all(
            getattr(before, metadata_field) == getattr(after, metadata_field)
            for metadata_field in stable_fields
        )
        require_condition(
            f"{binding_prefix}.stable_read",
            path=expected_path,
            ok=stable_read,
            ok_msg=f"{field} remained stable while read",
            fail_msg=f"{field} changed while its binding was validated",
            strict=True,
            remediation=f"Stop concurrent writers to {field} and rerun verification.",
        )
        if not stable_read:
            return

        actual_size = len(raw_bytes)
        actual_sha256 = f"sha256:{hashlib.sha256(raw_bytes).hexdigest()}"
        require_condition(
            f"{binding_prefix}.size_matches",
            path=expected_path,
            ok=bound_size == actual_size,
            ok_msg=f"{field} size_bytes matches {actual_size} raw bytes",
            fail_msg=(
                f"{field} size mismatch: bound={bound_size!r}, actual={actual_size}"
            ),
            strict=True,
            remediation=f"Recompute {field} binding after redaction and final writes.",
        )
        require_condition(
            f"{binding_prefix}.sha256_matches",
            path=expected_path,
            ok=bound_sha256 == actual_sha256,
            ok_msg=f"{field} SHA-256 matches retained raw bytes",
            fail_msg=(
                f"{field} SHA-256 mismatch: bound={bound_sha256!r}, actual={actual_sha256}"
            ),
            strict=True,
            remediation=f"Recompute {field} binding after redaction and final writes.",
        )

    def resolve_required_path_field(
        field: str,
        *,
        expected: Path,
        remediation_hint: str,
    ) -> Path | None:
        raw_value = result.get(field)
        raw_text = str(raw_value).strip() if raw_value is not None else ""
        require_condition(
            f"{check_prefix}:result.{field}_nonempty",
            path=result_path,
            ok=bool(raw_text),
            ok_msg=f"{field} is non-empty",
            fail_msg=f"{field} is missing or empty",
            strict=True,
            remediation=remediation_hint,
        )
        if not raw_text:
            return None
        resolved = Path(raw_text)
        require_condition(
            f"{check_prefix}:result.{field}_path_matches",
            path=result_path,
            ok=path_matches(raw_text, expected),
            ok_msg=f"{field} path matches expected artifact path",
            fail_msg=(
                f"{field} path mismatch; expected {expected}, got {raw_value!r}"
            ),
            strict=True,
            remediation=remediation_hint,
        )
        return resolved

    log_file = resolve_required_path_field(
        "log_file",
        expected=expected_log_path,
        remediation_hint="Write log_file path directly from the per-suite/per-target log path variable.",
    )
    if log_file is not None:
        require_condition(
            f"{check_prefix}:result.log_file_exists",
            path=log_file,
            ok=log_file.exists(),
            ok_msg="referenced log file exists",
            fail_msg=f"missing log file {log_file}",
            strict=True,
            remediation="Ensure cargo test output is tee'd into the declared log_file path.",
        )
        validate_file_budget(
            check_id=f"{check_prefix}:result.log_file_budget",
            path=log_file,
            max_bytes=MAX_OUTPUT_LOG_BYTES,
            strict=True,
            remediation=(
                "Reduce output.log verbosity or split suite execution when logs exceed budget."
            ),
        )
        validate_text_redaction(
            check_id=f"{check_prefix}:result.log_file_redaction",
            path=log_file,
            strict=True,
        )
    validate_diagnostic_binding("output_log", expected_log_path)

    if (expected_test_log_path is None) != (expected_artifact_index_path is None):
        raise RuntimeError("structured diagnostic paths must both be configured or both omitted")
    if expected_test_log_path is None and expected_artifact_index_path is None:
        for field in ("test_log_jsonl", "artifact_index_jsonl"):
            require_condition(
                f"{check_prefix}:result.diagnostic_artifacts.{field}_null",
                path=result_path,
                ok=isinstance(diagnostic_artifacts, dict)
                and diagnostic_artifacts.get(field) is None,
                ok_msg=f"{field} is explicitly unavailable for {kind} results",
                fail_msg=f"{field} must be null for {kind} results",
                strict=True,
            )
        return result

    test_log_jsonl_path = resolve_required_path_field(
        "test_log_jsonl",
        expected=expected_test_log_path,
        remediation_hint=(
            "Set TEST_LOG_JSONL_PATH to the suite/target-local path before running cargo test."
        ),
    )
    artifact_index_jsonl_path = resolve_required_path_field(
        "artifact_index_jsonl",
        expected=expected_artifact_index_path,
        remediation_hint=(
            "Set TEST_ARTIFACT_INDEX_PATH to the suite/target-local path before running cargo test."
        ),
    )

    if test_log_jsonl_path is None:
        return
    if artifact_index_jsonl_path is None:
        return

    validate_file_budget(
        check_id=f"{check_prefix}.test_log_jsonl.file_budget",
        path=test_log_jsonl_path,
        max_bytes=MAX_TEST_LOG_JSONL_BYTES,
        strict=True,
        remediation=(
            "Reduce test-log emission volume or shard runs to keep per-suite/target JSONL within budget."
        ),
    )
    validate_file_budget(
        check_id=f"{check_prefix}.artifact_index_jsonl.file_budget",
        path=artifact_index_jsonl_path,
        max_bytes=MAX_ARTIFACT_INDEX_JSONL_BYTES,
        strict=True,
        remediation=(
            "Reduce artifact-index emission volume or trim duplicate artifact writes."
        ),
    )
    validate_text_redaction(
        check_id=f"{check_prefix}.test_log_jsonl.redaction_scan",
        path=test_log_jsonl_path,
        strict=True,
    )
    validate_text_redaction(
        check_id=f"{check_prefix}.artifact_index_jsonl.redaction_scan",
        path=artifact_index_jsonl_path,
        strict=True,
    )
    validate_diagnostic_binding("test_log_jsonl", expected_test_log_path)
    validate_diagnostic_binding("artifact_index_jsonl", expected_artifact_index_path)

    trace_ids_by_test = parse_test_log_jsonl(check_prefix, test_log_jsonl_path)
    parse_artifact_index_jsonl(check_prefix, artifact_index_jsonl_path, trace_ids_by_test)

    normalized_test_log = normalized_jsonl_path(test_log_jsonl_path)
    normalized_artifact_index = normalized_jsonl_path(artifact_index_jsonl_path)
    validate_file_budget(
        check_id=f"{check_prefix}.test_log_jsonl.normalized_file_budget",
        path=normalized_test_log,
        max_bytes=MAX_TEST_LOG_JSONL_BYTES,
        strict=True,
        remediation=(
            "Ensure normalized test-log output remains bounded and does not duplicate excessive payload."
        ),
    )
    validate_file_budget(
        check_id=f"{check_prefix}.artifact_index_jsonl.normalized_file_budget",
        path=normalized_artifact_index,
        max_bytes=MAX_ARTIFACT_INDEX_JSONL_BYTES,
        strict=True,
        remediation=(
            "Ensure normalized artifact-index output remains bounded and deterministic."
        ),
    )
    validate_normalized_jsonl_pair(
        check_prefix=f"{check_prefix}.test_log_jsonl.normalized_contract",
        raw_path=test_log_jsonl_path,
        normalized_path=normalized_test_log,
        allowed_schemas=set(LOG_REQUIRED_FIELDS.keys()) | {"pi.test.artifact.v1"},
        enforce_path_placeholder=False,
    )
    validate_normalized_jsonl_pair(
        check_prefix=f"{check_prefix}.artifact_index_jsonl.normalized_contract",
        raw_path=artifact_index_jsonl_path,
        normalized_path=normalized_artifact_index,
        allowed_schemas={"pi.test.artifact.v1"},
        enforce_path_placeholder=True,
    )
    return result


def validate_failure_timeline_file(
    *,
    check_prefix: str,
    timeline_path: Path,
    suite_name: str,
    require_non_empty: bool,
) -> list[dict]:
    lines = read_jsonl_lines(f"{check_prefix}.timeline_jsonl", timeline_path, strict=True)
    require_condition(
        f"{check_prefix}.timeline_jsonl.non_empty",
        path=timeline_path,
        ok=(len(lines) > 0) if require_non_empty else True,
        ok_msg=f"timeline has {len(lines)} entries",
        fail_msg="timeline JSONL is empty",
        strict=True,
        remediation="Emit structured failure timeline events into failure_timeline.jsonl.",
    )
    records: list[dict] = []
    for line_no, line in lines:
        record_id = f"{check_prefix}.timeline_jsonl.line_{line_no}"
        try:
            payload = json.loads(line)
        except Exception as exc:  # pragma: no cover - defensive
            add_check(record_id, timeline_path, False, f"invalid JSON: {exc}")
            record_issue(
                record_id,
                f"invalid JSON: {exc}",
                strict=True,
                remediation="Write valid JSON objects per line in failure_timeline.jsonl.",
            )
            continue
        if not isinstance(payload, dict):
            add_check(record_id, timeline_path, False, "record must be an object")
            record_issue(
                record_id,
                "record must be an object",
                strict=True,
                remediation="Write object records (not arrays/scalars) in failure_timeline.jsonl.",
            )
            continue
        require_condition(
            f"{record_id}.schema",
            path=timeline_path,
            ok=payload.get("schema") == "pi.e2e.failure_timeline_event.v1",
            ok_msg="timeline schema matches",
            fail_msg=f"unexpected timeline schema {payload.get('schema')!r}",
            strict=True,
            remediation="Emit schema pi.e2e.failure_timeline_event.v1 for timeline events.",
        )
        require_condition(
            f"{record_id}.correlation_matches_summary",
            path=timeline_path,
            ok=str(payload.get("correlation_id", "")).strip() == summary_correlation_id,
            ok_msg="timeline correlation_id matches summary",
            fail_msg=(
                "timeline correlation_id mismatch: "
                f"expected {summary_correlation_id!r}, got {payload.get('correlation_id')!r}"
            ),
            strict=True,
            remediation="Propagate summary correlation_id into every timeline event.",
        )
        require_condition(
            f"{record_id}.suite_matches",
            path=timeline_path,
            ok=str(payload.get("suite", "")).strip() == suite_name,
            ok_msg="timeline suite matches expected suite",
            fail_msg=(
                f"timeline suite mismatch: expected {suite_name!r}, got {payload.get('suite')!r}"
            ),
            strict=True,
            remediation="Emit the suite identifier on every timeline event.",
        )
        require_condition(
            f"{record_id}.event_type_present",
            path=timeline_path,
            ok=bool(str(payload.get("event_type", "")).strip()),
            ok_msg="event_type present",
            fail_msg="timeline event missing event_type",
            strict=True,
            remediation="Set event_type for every timeline record.",
        )
        records.append(payload)
    return records

lib_result = validate_result_contract(
    kind="lib",
    name="lib",
    result_path=artifact_dir / "lib" / "result.json",
    expected_log_path=artifact_dir / "lib" / "output.log",
    expected_test_log_path=None,
    expected_artifact_index_path=None,
)
if isinstance(summary, dict):
    require_condition(
        "summary.lib_matches_result",
        path=summary_path,
        ok=isinstance(lib_result, dict) and summary.get("lib") == lib_result,
        ok_msg="summary embeds the exact lib result",
        fail_msg="summary lib result does not match lib/result.json",
        strict=True,
        remediation="Embed lib/result.json unchanged in write_summary().",
    )

for target in selected_units:
    validate_result_contract(
        kind="unit",
        name=target,
        result_path=artifact_dir / "unit" / target / "result.json",
        expected_log_path=artifact_dir / "unit" / target / "output.log",
        expected_test_log_path=artifact_dir / "unit" / target / "test-log.jsonl",
        expected_artifact_index_path=artifact_dir / "unit" / target / "artifact-index.jsonl",
    )

for suite in selected_suites:
    validate_result_contract(
        kind="suite",
        name=suite,
        result_path=artifact_dir / suite / "result.json",
        expected_log_path=artifact_dir / suite / "output.log",
        expected_test_log_path=artifact_dir / suite / "test-log.jsonl",
        expected_artifact_index_path=artifact_dir / suite / "artifact-index.jsonl",
    )


# 1b) Failure diagnostics artifacts (bd-1f42.8.6.4)
failed_suite_names: list[str] = []
if isinstance(summary, dict):
    suites_payload = summary.get("suites")
    if isinstance(suites_payload, list):
        for suite_payload in suites_payload:
            if not isinstance(suite_payload, dict):
                continue
            suite_name = str(suite_payload.get("suite", "")).strip()
            if not suite_name:
                continue
            try:
                suite_exit_code = int(suite_payload.get("exit_code"))
            except (TypeError, ValueError):
                suite_exit_code = 1
            if suite_exit_code != 0:
                failed_suite_names.append(suite_name)
failed_suite_names = sorted(set(failed_suite_names))

if isinstance(summary, dict):
    require_condition(
        "summary.failed_suites_matches_suite_results",
        path=summary_path,
        ok=int(summary.get("failed_suites") or 0) == len(failed_suite_names),
        ok_msg="summary.failed_suites matches suite result entries",
        fail_msg=(
            "summary.failed_suites mismatch: "
            f"expected {len(failed_suite_names)}, got {summary.get('failed_suites')!r}"
        ),
        strict=True,
        remediation="Compute failed_suites directly from suite result exit_code values in write_summary().",
    )

failure_meta = summary.get("failure_diagnostics") if isinstance(summary, dict) else None
require_condition(
    "summary.failure_diagnostics_object",
    path=summary_path,
    ok=isinstance(failure_meta, dict),
    ok_msg="summary includes failure_diagnostics metadata",
    fail_msg="summary missing failure_diagnostics metadata",
    strict=True,
    remediation="Run generate_failure_diagnostics() after write_summary().",
)

failure_meta_suites_by_name: dict[str, dict] = {}
expected_failure_index_path = artifact_dir / "failure_diagnostics_index.json"
expected_run_timeline_path = artifact_dir / "failure_timeline.jsonl"
if isinstance(failure_meta, dict):
    require_keys(
        "failure_diagnostics.summary_meta",
        failure_meta,
        summary_path,
        [
            "schema",
            "correlation_id",
            "index_path",
            "run_timeline_path",
            "failed_suite_count",
            "suites",
        ],
        strict=True,
    )
    require_condition(
        "failure_diagnostics.summary_meta.schema",
        path=summary_path,
        ok=failure_meta.get("schema") == "pi.e2e.failure_diagnostics.v1",
        ok_msg="failure diagnostics summary schema matches",
        fail_msg=(
            "expected summary.failure_diagnostics.schema 'pi.e2e.failure_diagnostics.v1', got "
            f"{failure_meta.get('schema')!r}"
        ),
        strict=True,
        remediation="Emit schema pi.e2e.failure_diagnostics.v1 in summary failure_diagnostics metadata.",
    )
    require_condition(
        "failure_diagnostics.summary_meta.correlation_id_matches",
        path=summary_path,
        ok=str(failure_meta.get("correlation_id", "")).strip() == summary_correlation_id,
        ok_msg="failure diagnostics summary correlation_id matches run correlation",
        fail_msg=(
            "summary.failure_diagnostics correlation_id mismatch: "
            f"expected {summary_correlation_id!r}, got {failure_meta.get('correlation_id')!r}"
        ),
        strict=True,
        remediation="Propagate summary correlation_id into summary.failure_diagnostics metadata.",
    )
    require_condition(
        "failure_diagnostics.summary_meta.index_path_matches",
        path=summary_path,
        ok=path_matches(failure_meta.get("index_path"), expected_failure_index_path),
        ok_msg="failure diagnostics index path matches expected artifact path",
        fail_msg=(
            "summary.failure_diagnostics.index_path does not match "
            f"{expected_failure_index_path}"
        ),
        strict=True,
        remediation="Set summary.failure_diagnostics.index_path from failure_diagnostics_index.json artifact path.",
    )
    require_condition(
        "failure_diagnostics.summary_meta.run_timeline_path_matches",
        path=summary_path,
        ok=path_matches(failure_meta.get("run_timeline_path"), expected_run_timeline_path),
        ok_msg="failure diagnostics run timeline path matches expected artifact path",
        fail_msg=(
            "summary.failure_diagnostics.run_timeline_path does not match "
            f"{expected_run_timeline_path}"
        ),
        strict=True,
        remediation="Set summary.failure_diagnostics.run_timeline_path from failure_timeline.jsonl path.",
    )
    require_condition(
        "failure_diagnostics.summary_meta.failed_suite_count",
        path=summary_path,
        ok=int(failure_meta.get("failed_suite_count") or 0) == len(failed_suite_names),
        ok_msg="failure diagnostics failed_suite_count matches failed suites",
        fail_msg=(
            "summary.failure_diagnostics.failed_suite_count mismatch: "
            f"expected {len(failed_suite_names)}, got {failure_meta.get('failed_suite_count')!r}"
        ),
        strict=True,
        remediation="Set failure_diagnostics.failed_suite_count from failed suite result count.",
    )
    suites_meta_payload = failure_meta.get("suites")
    require_condition(
        "failure_diagnostics.summary_meta.suites_array",
        path=summary_path,
        ok=isinstance(suites_meta_payload, list),
        ok_msg="summary failure diagnostics suites is an array",
        fail_msg="summary failure diagnostics suites must be an array",
        strict=True,
        remediation="Emit summary.failure_diagnostics.suites as a list of suite entries.",
    )
    if isinstance(suites_meta_payload, list):
        seen_suite_meta: set[str] = set()
        for index, suite_payload in enumerate(suites_meta_payload):
            check_prefix = f"failure_diagnostics.summary_meta.suites[{index}]"
            require_condition(
                f"{check_prefix}.object",
                path=summary_path,
                ok=isinstance(suite_payload, dict),
                ok_msg="suite diagnostics entry is object",
                fail_msg="suite diagnostics entry must be an object",
                strict=True,
            )
            if not isinstance(suite_payload, dict):
                continue
            require_keys(
                check_prefix,
                suite_payload,
                summary_path,
                [
                    "suite",
                    "digest_path",
                    "timeline_path",
                    "root_cause_class",
                    "impacted_scenario_ids",
                    "first_failing_assertion",
                ],
                strict=True,
            )
            suite_name = str(suite_payload.get("suite", "")).strip()
            require_condition(
                f"{check_prefix}.suite_nonempty",
                path=summary_path,
                ok=bool(suite_name),
                ok_msg="suite identifier is non-empty",
                fail_msg="suite identifier is missing/empty",
                strict=True,
            )
            if not suite_name:
                continue
            require_condition(
                f"{check_prefix}.suite_unique",
                path=summary_path,
                ok=suite_name not in seen_suite_meta,
                ok_msg=f"suite {suite_name} appears once",
                fail_msg=f"duplicate suite diagnostics entry for {suite_name}",
                strict=True,
            )
            seen_suite_meta.add(suite_name)
            failure_meta_suites_by_name[suite_name] = suite_payload
            expected_digest_path = artifact_dir / suite_name / "failure_digest.json"
            expected_timeline_path = artifact_dir / suite_name / "failure_timeline.jsonl"
            require_condition(
                f"{check_prefix}.digest_path_matches",
                path=summary_path,
                ok=path_matches(suite_payload.get("digest_path"), expected_digest_path),
                ok_msg="digest_path matches expected suite digest artifact",
                fail_msg=(
                    f"digest_path mismatch for suite {suite_name}: expected {expected_digest_path}"
                ),
                strict=True,
                remediation="Emit digest_path as <artifact_dir>/<suite>/failure_digest.json.",
            )
            require_condition(
                f"{check_prefix}.timeline_path_matches",
                path=summary_path,
                ok=path_matches(suite_payload.get("timeline_path"), expected_timeline_path),
                ok_msg="timeline_path matches expected suite timeline artifact",
                fail_msg=(
                    f"timeline_path mismatch for suite {suite_name}: expected {expected_timeline_path}"
                ),
                strict=True,
                remediation="Emit timeline_path as <artifact_dir>/<suite>/failure_timeline.jsonl.",
            )
            impacted = suite_payload.get("impacted_scenario_ids")
            require_condition(
                f"{check_prefix}.impacted_scenario_ids_non_empty",
                path=summary_path,
                ok=isinstance(impacted, list) and len(impacted) > 0,
                ok_msg="impacted_scenario_ids present with at least one scenario id",
                fail_msg=f"impacted_scenario_ids missing/empty for suite {suite_name}",
                strict=suite_name in failed_suite_names,
                remediation="Populate impacted_scenario_ids with failing test/scenario identifiers.",
            )

require_condition(
    "failure_diagnostics.summary_meta.covers_failed_suites",
    path=summary_path,
    ok=sorted(failure_meta_suites_by_name.keys()) == failed_suite_names,
    ok_msg="summary failure diagnostics entries cover all failed suites",
    fail_msg=(
        "summary failure diagnostics entries do not match failed suites: "
        f"expected {failed_suite_names}, got {sorted(failure_meta_suites_by_name.keys())}"
    ),
    strict=True,
    remediation="Emit one summary.failure_diagnostics.suites entry per failed suite.",
)

failure_index_payload = load_json(
    "failure_diagnostics.index_json",
    expected_failure_index_path,
    strict=True,
)
require_keys(
    "failure_diagnostics.index_json",
    failure_index_payload,
    expected_failure_index_path,
    [
        "schema",
        "generated_at",
        "correlation_id",
        "artifact_dir",
        "failed_suite_count",
        "suites",
        "run_timeline_path",
    ],
    strict=True,
)

index_suites_by_name: dict[str, dict] = {}
if isinstance(failure_index_payload, dict):
    require_condition(
        "failure_diagnostics.index_json.schema",
        path=expected_failure_index_path,
        ok=failure_index_payload.get("schema") == "pi.e2e.failure_diagnostics_index.v1",
        ok_msg="failure diagnostics index schema matches",
        fail_msg=(
            "expected failure diagnostics index schema 'pi.e2e.failure_diagnostics_index.v1', got "
            f"{failure_index_payload.get('schema')!r}"
        ),
        strict=True,
        remediation="Emit schema pi.e2e.failure_diagnostics_index.v1 in failure_diagnostics_index.json.",
    )
    require_condition(
        "failure_diagnostics.index_json.correlation_matches_summary",
        path=expected_failure_index_path,
        ok=str(failure_index_payload.get("correlation_id", "")).strip() == summary_correlation_id,
        ok_msg="failure diagnostics index correlation_id matches summary",
        fail_msg=(
            "failure diagnostics index correlation_id mismatch: "
            f"expected {summary_correlation_id!r}, got {failure_index_payload.get('correlation_id')!r}"
        ),
        strict=True,
        remediation="Propagate summary correlation_id into failure_diagnostics_index.json.",
    )
    require_condition(
        "failure_diagnostics.index_json.artifact_dir_matches",
        path=expected_failure_index_path,
        ok=path_matches(failure_index_payload.get("artifact_dir"), artifact_dir),
        ok_msg="failure diagnostics index artifact_dir matches run artifact directory",
        fail_msg=(
            "failure diagnostics index artifact_dir mismatch: "
            f"expected {artifact_dir}, got {failure_index_payload.get('artifact_dir')!r}"
        ),
        strict=True,
        remediation="Write artifact_dir in failure_diagnostics_index.json from ARTIFACT_DIR.",
    )
    require_condition(
        "failure_diagnostics.index_json.failed_suite_count",
        path=expected_failure_index_path,
        ok=int(failure_index_payload.get("failed_suite_count") or 0) == len(failed_suite_names),
        ok_msg="failure diagnostics index failed_suite_count matches failed suites",
        fail_msg=(
            "failure diagnostics index failed_suite_count mismatch: "
            f"expected {len(failed_suite_names)}, got {failure_index_payload.get('failed_suite_count')!r}"
        ),
        strict=True,
        remediation="Set failed_suite_count in failure_diagnostics_index.json from failed suite result count.",
    )
    require_condition(
        "failure_diagnostics.index_json.run_timeline_path_matches",
        path=expected_failure_index_path,
        ok=path_matches(failure_index_payload.get("run_timeline_path"), expected_run_timeline_path),
        ok_msg="failure diagnostics index run_timeline_path matches expected path",
        fail_msg=(
            "failure diagnostics index run_timeline_path mismatch: "
            f"expected {expected_run_timeline_path}, got {failure_index_payload.get('run_timeline_path')!r}"
        ),
        strict=True,
        remediation="Set run_timeline_path in failure_diagnostics_index.json from failure_timeline.jsonl.",
    )
    index_suites_payload = failure_index_payload.get("suites")
    require_condition(
        "failure_diagnostics.index_json.suites_array",
        path=expected_failure_index_path,
        ok=isinstance(index_suites_payload, list),
        ok_msg="failure diagnostics index suites is an array",
        fail_msg="failure diagnostics index suites must be an array",
        strict=True,
    )
    if isinstance(index_suites_payload, list):
        seen_index_suites: set[str] = set()
        for index, suite_payload in enumerate(index_suites_payload):
            check_prefix = f"failure_diagnostics.index_json.suites[{index}]"
            require_condition(
                f"{check_prefix}.object",
                path=expected_failure_index_path,
                ok=isinstance(suite_payload, dict),
                ok_msg="index suite entry is object",
                fail_msg="index suite entry must be an object",
                strict=True,
            )
            if not isinstance(suite_payload, dict):
                continue
            require_keys(
                check_prefix,
                suite_payload,
                expected_failure_index_path,
                [
                    "suite",
                    "digest_path",
                    "timeline_path",
                    "root_cause_class",
                    "impacted_scenario_ids",
                    "first_failing_assertion",
                ],
                strict=True,
            )
            suite_name = str(suite_payload.get("suite", "")).strip()
            require_condition(
                f"{check_prefix}.suite_nonempty",
                path=expected_failure_index_path,
                ok=bool(suite_name),
                ok_msg="suite identifier is non-empty",
                fail_msg="suite identifier missing/empty",
                strict=True,
            )
            if not suite_name:
                continue
            require_condition(
                f"{check_prefix}.suite_unique",
                path=expected_failure_index_path,
                ok=suite_name not in seen_index_suites,
                ok_msg=f"suite {suite_name} appears once in index",
                fail_msg=f"duplicate suite entry in index for {suite_name}",
                strict=True,
            )
            seen_index_suites.add(suite_name)
            index_suites_by_name[suite_name] = suite_payload

require_condition(
    "failure_diagnostics.index_json.covers_failed_suites",
    path=expected_failure_index_path,
    ok=sorted(index_suites_by_name.keys()) == failed_suite_names,
    ok_msg="failure diagnostics index covers all failed suites",
    fail_msg=(
        "failure diagnostics index suites do not match failed suites: "
        f"expected {failed_suite_names}, got {sorted(index_suites_by_name.keys())}"
    ),
    strict=True,
)

run_timeline_lines = read_jsonl_lines(
    "failure_diagnostics.run.timeline_jsonl",
    expected_run_timeline_path,
    strict=True,
)
require_condition(
    "failure_diagnostics.run.timeline_jsonl.non_empty",
    path=expected_run_timeline_path,
    ok=len(run_timeline_lines) > 0,
    ok_msg=f"run timeline has {len(run_timeline_lines)} entries",
    fail_msg="run timeline JSONL is empty",
    strict=True,
    remediation="Emit run-level failure timeline events in failure_timeline.jsonl.",
)
for line_no, line in run_timeline_lines:
    record_id = f"failure_diagnostics.run.timeline_jsonl.line_{line_no}"
    try:
        payload = json.loads(line)
    except Exception as exc:  # pragma: no cover - defensive
        add_check(record_id, expected_run_timeline_path, False, f"invalid JSON: {exc}")
        record_issue(
            record_id,
            f"invalid JSON: {exc}",
            strict=True,
            remediation="Write valid JSON records in run failure_timeline.jsonl.",
        )
        continue
    if not isinstance(payload, dict):
        add_check(record_id, expected_run_timeline_path, False, "record must be an object")
        record_issue(
            record_id,
            "record must be an object",
            strict=True,
            remediation="Write object records in run failure_timeline.jsonl.",
        )
        continue
    require_condition(
        f"{record_id}.schema",
        path=expected_run_timeline_path,
        ok=payload.get("schema") == "pi.e2e.failure_timeline_event.v1",
        ok_msg="run timeline schema matches",
        fail_msg=f"unexpected run timeline schema {payload.get('schema')!r}",
        strict=True,
    )
    require_condition(
        f"{record_id}.correlation_matches_summary",
        path=expected_run_timeline_path,
        ok=str(payload.get("correlation_id", "")).strip() == summary_correlation_id,
        ok_msg="run timeline correlation_id matches summary",
        fail_msg=(
            "run timeline correlation_id mismatch: "
            f"expected {summary_correlation_id!r}, got {payload.get('correlation_id')!r}"
        ),
        strict=True,
    )
    require_condition(
        f"{record_id}.suite_present",
        path=expected_run_timeline_path,
        ok=bool(str(payload.get("suite", "")).strip()),
        ok_msg="run timeline event has suite field",
        fail_msg="run timeline event missing suite field",
        strict=True,
    )

for suite_name in failed_suite_names:
    suite_prefix = f"failure_diagnostics.{suite_name}"
    expected_digest_path = artifact_dir / suite_name / "failure_digest.json"
    expected_timeline_path = artifact_dir / suite_name / "failure_timeline.jsonl"

    meta_entry = failure_meta_suites_by_name.get(suite_name)
    require_condition(
        f"{suite_prefix}.summary_meta_present",
        path=summary_path,
        ok=isinstance(meta_entry, dict),
        ok_msg=f"summary metadata entry exists for {suite_name}",
        fail_msg=f"summary missing failure diagnostics entry for suite {suite_name}",
        strict=True,
    )
    if isinstance(meta_entry, dict):
        require_condition(
            f"{suite_prefix}.summary_meta.digest_path_matches",
            path=summary_path,
            ok=path_matches(meta_entry.get("digest_path"), expected_digest_path),
            ok_msg="summary metadata digest_path matches expected suite digest path",
            fail_msg=f"summary metadata digest_path mismatch for suite {suite_name}",
            strict=True,
        )
        require_condition(
            f"{suite_prefix}.summary_meta.timeline_path_matches",
            path=summary_path,
            ok=path_matches(meta_entry.get("timeline_path"), expected_timeline_path),
            ok_msg="summary metadata timeline_path matches expected suite timeline path",
            fail_msg=f"summary metadata timeline_path mismatch for suite {suite_name}",
            strict=True,
        )

    index_entry = index_suites_by_name.get(suite_name)
    require_condition(
        f"{suite_prefix}.index_entry_present",
        path=expected_failure_index_path,
        ok=isinstance(index_entry, dict),
        ok_msg=f"index entry exists for {suite_name}",
        fail_msg=f"failure diagnostics index missing suite entry for {suite_name}",
        strict=True,
    )
    if isinstance(index_entry, dict):
        require_condition(
            f"{suite_prefix}.index_entry.digest_path_matches",
            path=expected_failure_index_path,
            ok=path_matches(index_entry.get("digest_path"), expected_digest_path),
            ok_msg="index digest_path matches expected suite digest path",
            fail_msg=f"index digest_path mismatch for suite {suite_name}",
            strict=True,
        )
        require_condition(
            f"{suite_prefix}.index_entry.timeline_path_matches",
            path=expected_failure_index_path,
            ok=path_matches(index_entry.get("timeline_path"), expected_timeline_path),
            ok_msg="index timeline_path matches expected suite timeline path",
            fail_msg=f"index timeline_path mismatch for suite {suite_name}",
            strict=True,
        )

    digest_payload = load_json(
        f"{suite_prefix}.digest_json",
        expected_digest_path,
        strict=True,
    )
    require_keys(
        f"{suite_prefix}.digest_json",
        digest_payload,
        expected_digest_path,
        [
            "schema",
            "generated_at",
            "correlation_id",
            "suite",
            "exit_code",
            "root_cause_class",
            "impacted_scenario_ids",
            "first_failing_assertion",
            "remediation_pointer",
            "artifact_paths",
            "timeline",
        ],
        strict=True,
    )
    if isinstance(digest_payload, dict):
        require_condition(
            f"{suite_prefix}.digest_json.schema",
            path=expected_digest_path,
            ok=digest_payload.get("schema") == "pi.e2e.failure_digest.v1",
            ok_msg="failure digest schema matches",
            fail_msg=f"unexpected failure digest schema {digest_payload.get('schema')!r}",
            strict=True,
            remediation="Emit schema pi.e2e.failure_digest.v1 in failure_digest.json.",
        )
        require_condition(
            f"{suite_prefix}.digest_json.correlation_matches_summary",
            path=expected_digest_path,
            ok=str(digest_payload.get("correlation_id", "")).strip() == summary_correlation_id,
            ok_msg="failure digest correlation_id matches summary",
            fail_msg=(
                "failure digest correlation_id mismatch: "
                f"expected {summary_correlation_id!r}, got {digest_payload.get('correlation_id')!r}"
            ),
            strict=True,
            remediation="Propagate summary correlation_id into failure_digest.json.",
        )
        require_condition(
            f"{suite_prefix}.digest_json.suite_matches",
            path=expected_digest_path,
            ok=str(digest_payload.get("suite", "")).strip() == suite_name,
            ok_msg="failure digest suite matches expected suite",
            fail_msg=(
                f"failure digest suite mismatch: expected {suite_name!r}, got {digest_payload.get('suite')!r}"
            ),
            strict=True,
            remediation="Emit suite identifier in failure_digest.json from failing suite name.",
        )
        impacted = digest_payload.get("impacted_scenario_ids")
        require_condition(
            f"{suite_prefix}.digest_json.impacted_scenario_ids_non_empty",
            path=expected_digest_path,
            ok=isinstance(impacted, list) and len(impacted) > 0,
            ok_msg="impacted_scenario_ids contains at least one scenario id",
            fail_msg="failure digest impacted_scenario_ids missing/empty",
            strict=True,
            remediation="Populate impacted_scenario_ids from failing tests listed in output.log.",
        )
        first_assertion = digest_payload.get("first_failing_assertion")
        require_condition(
            f"{suite_prefix}.digest_json.first_failing_assertion_object",
            path=expected_digest_path,
            ok=isinstance(first_assertion, dict),
            ok_msg="first_failing_assertion is an object",
            fail_msg="first_failing_assertion missing or invalid",
            strict=True,
            remediation="Emit first_failing_assertion object with test, location, and message.",
        )
        if isinstance(first_assertion, dict):
            require_condition(
                f"{suite_prefix}.digest_json.first_failing_assertion.test",
                path=expected_digest_path,
                ok=bool(str(first_assertion.get("test", "")).strip()),
                ok_msg="first_failing_assertion.test is present",
                fail_msg="first_failing_assertion.test missing/empty",
                strict=True,
                remediation="Set first_failing_assertion.test from first failing test id.",
            )
            require_condition(
                f"{suite_prefix}.digest_json.first_failing_assertion.message",
                path=expected_digest_path,
                ok=bool(str(first_assertion.get("message", "")).strip()),
                ok_msg="first_failing_assertion.message is present",
                fail_msg="first_failing_assertion.message missing/empty",
                strict=True,
                remediation="Set first_failing_assertion.message from first panic/assertion message.",
            )
            location_payload = first_assertion.get("location")
            require_condition(
                f"{suite_prefix}.digest_json.first_failing_assertion.location_object",
                path=expected_digest_path,
                ok=isinstance(location_payload, dict),
                ok_msg="first_failing_assertion.location is an object",
                fail_msg="first_failing_assertion.location missing/invalid",
                strict=True,
                remediation="Emit location object with file/line/column for first failing assertion.",
            )
            if isinstance(location_payload, dict):
                for field in ("file", "line", "column"):
                    require_condition(
                        f"{suite_prefix}.digest_json.first_failing_assertion.location.{field}",
                        path=expected_digest_path,
                        ok=field in location_payload,
                        ok_msg=f"location.{field} present",
                        fail_msg=f"missing location.{field}",
                        strict=True,
                    )

        remediation_pointer = digest_payload.get("remediation_pointer")
        require_condition(
            f"{suite_prefix}.digest_json.remediation_pointer_object",
            path=expected_digest_path,
            ok=isinstance(remediation_pointer, dict),
            ok_msg="remediation_pointer is an object",
            fail_msg="remediation_pointer missing or invalid",
            strict=True,
            remediation="Emit remediation_pointer with replay command pointers.",
        )
        if isinstance(remediation_pointer, dict):
            for field in (
                "class",
                "summary",
                "replay_command",
                "suite_replay_command",
                "targeted_test_replay_command",
            ):
                require_condition(
                    f"{suite_prefix}.digest_json.remediation_pointer.{field}",
                    path=expected_digest_path,
                    ok=field in remediation_pointer,
                    ok_msg=f"remediation_pointer.{field} present",
                    fail_msg=f"missing remediation_pointer.{field}",
                    strict=True,
                )

        artifact_paths = digest_payload.get("artifact_paths")
        require_condition(
            f"{suite_prefix}.digest_json.artifact_paths_object",
            path=expected_digest_path,
            ok=isinstance(artifact_paths, dict),
            ok_msg="artifact_paths is an object",
            fail_msg="artifact_paths missing or invalid",
            strict=True,
            remediation="Emit artifact_paths object in failure_digest.json.",
        )
        if isinstance(artifact_paths, dict):
            for field in (
                "result_json",
                "output_log",
                "test_log_jsonl",
                "artifact_index_jsonl",
                "timeline_jsonl",
                "runner_summary_json",
            ):
                require_condition(
                    f"{suite_prefix}.digest_json.artifact_paths.{field}",
                    path=expected_digest_path,
                    ok=field in artifact_paths,
                    ok_msg=f"artifact_paths.{field} present",
                    fail_msg=f"missing artifact_paths.{field}",
                    strict=True,
                )
            require_condition(
                f"{suite_prefix}.digest_json.artifact_paths.timeline_path_matches",
                path=expected_digest_path,
                ok=path_matches(artifact_paths.get("timeline_jsonl"), expected_timeline_path),
                ok_msg="artifact_paths.timeline_jsonl matches suite timeline path",
                fail_msg="artifact_paths.timeline_jsonl does not match expected suite timeline path",
                strict=True,
                remediation="Set artifact_paths.timeline_jsonl to <artifact_dir>/<suite>/failure_timeline.jsonl.",
            )

        timeline_meta = digest_payload.get("timeline")
        require_condition(
            f"{suite_prefix}.digest_json.timeline_object",
            path=expected_digest_path,
            ok=isinstance(timeline_meta, dict),
            ok_msg="timeline metadata object exists",
            fail_msg="timeline metadata missing or invalid",
            strict=True,
            remediation="Emit timeline metadata with schema/path/event_count in failure_digest.json.",
        )
        if isinstance(timeline_meta, dict):
            require_condition(
                f"{suite_prefix}.digest_json.timeline.schema",
                path=expected_digest_path,
                ok=timeline_meta.get("schema") == "pi.e2e.failure_timeline_event.v1",
                ok_msg="timeline metadata schema matches",
                fail_msg=(
                    "timeline metadata schema mismatch: expected "
                    "'pi.e2e.failure_timeline_event.v1'"
                ),
                strict=True,
            )
            require_condition(
                f"{suite_prefix}.digest_json.timeline.path_matches",
                path=expected_digest_path,
                ok=path_matches(timeline_meta.get("path"), expected_timeline_path),
                ok_msg="timeline metadata path matches suite timeline path",
                fail_msg="timeline metadata path does not match expected suite timeline path",
                strict=True,
                remediation="Set timeline.path in failure_digest.json from suite timeline artifact path.",
            )

        timeline_records = validate_failure_timeline_file(
            check_prefix=suite_prefix,
            timeline_path=expected_timeline_path,
            suite_name=suite_name,
            require_non_empty=True,
        )
        if isinstance(timeline_meta, dict):
            event_count_value = timeline_meta.get("event_count")
            require_condition(
                f"{suite_prefix}.digest_json.timeline.event_count_matches",
                path=expected_digest_path,
                ok=isinstance(event_count_value, int) and event_count_value == len(timeline_records),
                ok_msg="timeline metadata event_count matches timeline record count",
                fail_msg=(
                    "timeline metadata event_count mismatch: "
                    f"expected {len(timeline_records)}, got {event_count_value!r}"
                ),
                strict=True,
                remediation="Set timeline.event_count to the number of emitted timeline JSONL records.",
            )


# 2) Conformance evidence artifacts (strict for full baseline profile)
reports_dir = project_root / "tests" / "ext_conformance" / "reports"
events_path = reports_dir / "conformance_events.jsonl"
summary_report_path = reports_dir / "conformance_summary.json"
markdown_path = reports_dir / "CONFORMANCE_REPORT.md"

events_exists = require_file(
    "conformance.events_jsonl",
    events_path,
    strict=strict_conformance,
    description="per-extension JSONL evidence log exists",
)
summary_report = load_json(
    "conformance.summary_json",
    summary_report_path,
    strict=strict_conformance,
)
require_keys(
    "conformance.summary_json",
    summary_report,
    summary_report_path,
    [
        "schema",
        "generated_at",
        "run_id",
        "correlation_id",
        "counts",
        "pass_rate_pct",
        "evidence",
    ],
    strict=strict_conformance,
)

if isinstance(summary_report, dict):
    conformance_run_id = str(summary_report.get("run_id", "")).strip()
    require_condition(
        "conformance.summary_run_id_nonempty",
        path=summary_report_path,
        ok=bool(conformance_run_id),
        ok_msg="conformance summary run_id is set",
        fail_msg="conformance summary run_id is empty",
        strict=strict_conformance,
        remediation=(
            "Emit conformance_summary.run_id from the latest canonical consolidated "
            "conformance run."
        ),
    )

    conformance_correlation_id = str(summary_report.get("correlation_id", "")).strip()
    require_condition(
        "conformance.summary_correlation_id_nonempty",
        path=summary_report_path,
        ok=bool(conformance_correlation_id),
        ok_msg="conformance summary correlation_id is set",
        fail_msg="conformance summary correlation_id is empty",
        strict=strict_conformance,
        remediation=(
            "Emit conformance_summary.correlation_id and keep it aligned with "
            "run_all summary correlation_id."
        ),
    )
    require_condition(
        "conformance.summary_correlation_id_matches_summary",
        path=summary_report_path,
        ok=bool(summary_correlation_id)
        and conformance_correlation_id == summary_correlation_id,
        ok_msg="conformance summary correlation_id matches run summary correlation_id",
        fail_msg=(
            "conformance summary correlation_id mismatch: expected "
            f"{summary_correlation_id!r}, got {conformance_correlation_id!r}"
        ),
        strict=strict_conformance,
        remediation=(
            "Set conformance_summary.correlation_id from run_all summary correlation_id "
            "for canonical consolidated lineage."
        ),
    )

    evidence_payload = summary_report.get("evidence")
    require_condition(
        "conformance.summary_json.evidence_object",
        path=summary_report_path,
        ok=isinstance(evidence_payload, dict),
        ok_msg="evidence payload is an object",
        fail_msg="missing or invalid evidence object",
        strict=strict_conformance,
    )
    if isinstance(evidence_payload, dict):
        missing_evidence_keys = sorted(
            set(["golden_fixtures", "load_time_benchmarks", "parity_logs", "smoke_logs"]) -
            set(evidence_payload.keys())
        )
        require_condition(
            "conformance.summary_json.evidence_keys",
            path=summary_report_path,
            ok=not missing_evidence_keys,
            ok_msg="evidence payload includes required keys",
            fail_msg=f"missing evidence keys: {missing_evidence_keys}",
            strict=strict_conformance,
        )

    schema = summary_report.get("schema")
    schema_ok = schema == "pi.ext.conformance_summary.v2"
    add_check(
        "conformance.summary_schema",
        summary_report_path,
        schema_ok,
        f"schema={schema!r}" if schema_ok else f"unexpected schema {schema!r}",
    )
    if not schema_ok:
        message = (
            f"conformance.summary_schema: expected 'pi.ext.conformance_summary.v2', got {schema!r}"
        )
        if strict_conformance:
            errors.append(message)
        else:
            warnings.append(message)

markdown_exists = require_file(
    "conformance.report_markdown",
    markdown_path,
    strict=strict_conformance,
    description="human-readable conformance report exists",
)
if markdown_exists:
    content = markdown_path.read_text(encoding="utf-8")
    has_index = "## Evidence Index" in content
    add_check(
        "conformance.report_markdown.evidence_index",
        markdown_path,
        has_index,
        "contains Evidence Index section" if has_index else "missing '## Evidence Index'",
    )
    if not has_index:
        message = f"conformance.report_markdown: missing Evidence Index in {markdown_path}"
        if strict_conformance:
            errors.append(message)
        else:
            warnings.append(message)

if events_exists:
    try:
        lines = [line for line in events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    except Exception as exc:  # pragma: no cover - defensive
        add_check("conformance.events_jsonl.readable", events_path, False, f"read failed: {exc}")
        if strict_conformance:
            errors.append(f"conformance.events_jsonl: read failed ({exc})")
        else:
            warnings.append(f"conformance.events_jsonl: read failed ({exc})")
        lines = []
    if lines:
        add_check("conformance.events_jsonl.non_empty", events_path, True, f"{len(lines)} line(s)")
        first_invalid = None
        for index, line in enumerate(lines, start=1):
            try:
                json.loads(line)
            except Exception as exc:  # pragma: no cover - defensive
                first_invalid = f"line {index}: {exc}"
                break
        require_condition(
            "conformance.events_jsonl.valid_json",
            path=events_path,
            ok=first_invalid is None,
            ok_msg="every JSONL entry parsed successfully",
            fail_msg=f"invalid JSONL entry ({first_invalid})",
            strict=strict_conformance,
        )
    else:
        add_check("conformance.events_jsonl.non_empty", events_path, False, "no JSONL entries found")
        message = f"conformance.events_jsonl: empty file {events_path}"
        if strict_conformance:
            errors.append(message)
        else:
            warnings.append(message)

# 3) Capability-profile matrix evidence (required when conformance events are available)
profile_matrix_path = artifact_dir / "extension_profile_matrix.json"
if not profile_matrix_path.exists() and not strict_conformance:
    add_check(
        "conformance.profile_matrix_json",
        profile_matrix_path,
        True,
        "skipped (conformance events not available in this profile)",
    )
    profile_matrix = None
else:
    profile_matrix = load_json(
        "conformance.profile_matrix_json",
        profile_matrix_path,
        strict=True,
    )
require_keys(
    "conformance.profile_matrix_json",
    profile_matrix,
    profile_matrix_path,
    ["schema", "generated_at", "profiles", "preflight_runtime_consistency"],
    strict=True,
)

if isinstance(profile_matrix, dict):
    schema = profile_matrix.get("schema")
    require_condition(
        "conformance.profile_matrix_schema",
        path=profile_matrix_path,
        ok=schema == "pi.e2e.extension_profile_matrix.v1",
        ok_msg="profile matrix schema matches",
        fail_msg=(
            "expected schema 'pi.e2e.extension_profile_matrix.v1', "
            f"got {schema!r}"
        ),
        strict=True,
    )

    profiles_payload = profile_matrix.get("profiles")
    profiles_ok = isinstance(profiles_payload, dict)
    require_condition(
        "conformance.profile_matrix_profiles_object",
        path=profile_matrix_path,
        ok=profiles_ok,
        ok_msg="profiles object exists",
        fail_msg="profiles must be an object",
        strict=True,
    )

    if profiles_ok:
        required_profiles = ["safe", "balanced", "permissive"]
        missing_profiles = [
            profile_name
            for profile_name in required_profiles
            if profile_name not in profiles_payload
        ]
        require_condition(
            "conformance.profile_matrix_profiles_complete",
            path=profile_matrix_path,
            ok=not missing_profiles,
            ok_msg="safe|balanced|permissive profile entries exist",
            fail_msg=f"missing profile entries: {missing_profiles}",
            strict=True,
        )

        for profile_name in required_profiles:
            profile_entry = profiles_payload.get(profile_name)
            prefix = f"conformance.profile_matrix.{profile_name}"
            require_condition(
                f"{prefix}.object",
                path=profile_matrix_path,
                ok=isinstance(profile_entry, dict),
                ok_msg=f"{profile_name} profile entry is object",
                fail_msg=f"{profile_name} profile entry must be object",
                strict=True,
            )
            if not isinstance(profile_entry, dict):
                continue

            projected = profile_entry.get("projected")
            require_condition(
                f"{prefix}.projected_object",
                path=profile_matrix_path,
                ok=isinstance(projected, dict),
                ok_msg="projected object exists",
                fail_msg="projected must be an object",
                strict=True,
            )

            blocked_summary = profile_entry.get("blocked_operation_summary")
            require_condition(
                f"{prefix}.blocked_summary",
                path=profile_matrix_path,
                ok=isinstance(blocked_summary, dict),
                ok_msg="blocked_operation_summary object exists",
                fail_msg="blocked_operation_summary must be an object",
                strict=True,
            )

            if isinstance(projected, dict):
                for field in (
                    "testable_extensions",
                    "pass",
                    "fail_existing",
                    "blocked",
                    "pass_rate_pct",
                ):
                    require_condition(
                        f"{prefix}.projected.{field}",
                        path=profile_matrix_path,
                        ok=field in projected,
                        ok_msg=f"projected.{field} present",
                        fail_msg=f"missing projected.{field}",
                        strict=True,
                    )

    consistency = profile_matrix.get("preflight_runtime_consistency")
    require_condition(
        "conformance.profile_matrix_consistency_object",
        path=profile_matrix_path,
        ok=isinstance(consistency, dict),
        ok_msg="preflight_runtime_consistency object exists",
        fail_msg="preflight_runtime_consistency must be an object",
        strict=True,
    )
    if isinstance(consistency, dict):
        checked = consistency.get("checked_capability_decisions")
        mismatched = consistency.get("mismatched_capability_decisions")
        require_condition(
            "conformance.profile_matrix_consistency_checked_nonzero",
            path=profile_matrix_path,
            ok=isinstance(checked, int) and checked > 0,
            ok_msg=f"checked_capability_decisions={checked}",
            fail_msg=(
                "checked_capability_decisions must be a positive integer "
                f"(got {checked!r})"
            ),
            strict=True,
        )
        require_condition(
            "conformance.profile_matrix_consistency_no_mismatch",
            path=profile_matrix_path,
            ok=isinstance(mismatched, int) and mismatched == 0,
            ok_msg="preflight/runtime consistency mismatches == 0",
            fail_msg=(
                "preflight/runtime consistency mismatches must be 0 "
                f"(got {mismatched!r})"
            ),
            strict=True,
        )


# 4) Soak/longevity evidence (bd-k5q5.7.10)
soak_report_path = artifact_dir / "soak_longevity_report.json"
soak_report = load_json(
    "conformance.soak_longevity_report_json",
    soak_report_path,
    strict=True,
)
require_keys(
    "conformance.soak_longevity_report_json",
    soak_report,
    soak_report_path,
    [
        "schema",
        "generated_at",
        "status",
        "pass",
        "prerequisites",
        "metrics",
        "thresholds",
        "checks",
        "failed_checks",
    ],
    strict=True,
)

if isinstance(soak_report, dict):
    soak_schema = soak_report.get("schema")
    require_condition(
        "conformance.soak_longevity_report_schema",
        path=soak_report_path,
        ok=soak_schema == "pi.e2e.soak_longevity.v1",
        ok_msg="soak/longevity schema matches",
        fail_msg=(
            "expected schema 'pi.e2e.soak_longevity.v1', "
            f"got {soak_schema!r}"
        ),
        strict=True,
    )

    prerequisites = soak_report.get("prerequisites")
    require_condition(
        "conformance.soak_longevity_prerequisites_object",
        path=soak_report_path,
        ok=isinstance(prerequisites, dict),
        ok_msg="prerequisites object exists",
        fail_msg="prerequisites must be an object",
        strict=True,
    )

    checks_payload = soak_report.get("checks")
    require_condition(
        "conformance.soak_longevity_checks_array",
        path=soak_report_path,
        ok=isinstance(checks_payload, list) and len(checks_payload) > 0,
        ok_msg=f"checks list contains {len(checks_payload) if isinstance(checks_payload, list) else 0} entries",
        fail_msg="checks must be a non-empty array",
        strict=True,
    )

    if strict_conformance and isinstance(prerequisites, dict):
        required_targets_executed = prerequisites.get("required_targets_executed")
        require_condition(
            "conformance.soak_longevity_required_targets_executed",
            path=soak_report_path,
            ok=required_targets_executed is True,
            ok_msg="required stress targets executed in this run",
            fail_msg=(
                "required stress targets were not executed in this run "
                "(expected ext_memory_stress + extensions_stress to pass)"
            ),
            strict=True,
        )

        report_pass = soak_report.get("pass")
        require_condition(
            "conformance.soak_longevity_pass",
            path=soak_report_path,
            ok=report_pass is True,
            ok_msg="soak/longevity verdict passed",
            fail_msg=f"soak/longevity verdict must pass for full baseline run (got {report_pass!r})",
            strict=True,
        )

soak_events_path = artifact_dir / "soak_longevity_events.jsonl"
soak_events_exists = require_file(
    "conformance.soak_longevity_events_jsonl",
    soak_events_path,
    strict=True,
    description="soak/longevity structured events log exists",
)
if soak_events_exists:
    lines = [line for line in soak_events_path.read_text(encoding="utf-8").splitlines() if line.strip()]
    require_condition(
        "conformance.soak_longevity_events_non_empty",
        path=soak_events_path,
        ok=len(lines) > 0,
        ok_msg=f"soak/longevity events contains {len(lines)} entries",
        fail_msg="soak/longevity events log is empty",
        strict=True,
    )


# 5) User-focused release-readiness summary (bd-k5q5.7.11)
release_readiness_path = artifact_dir / "release_readiness_summary.json"
release_readiness = load_json(
    "conformance.release_readiness_json",
    release_readiness_path,
    strict=True,
)
require_keys(
    "conformance.release_readiness_json",
    release_readiness,
    release_readiness_path,
    [
        "schema",
        "generated_at",
        "correlation_id",
        "status",
        "overall_ready",
        "summary",
        "profiles",
        "workflow_outcomes",
        "known_risks",
        "remediation",
        "evidence",
    ],
    strict=True,
)

if isinstance(release_readiness, dict):
    require_condition(
        "conformance.release_readiness_schema",
        path=release_readiness_path,
        ok=release_readiness.get("schema") == "pi.e2e.release_readiness.v1",
        ok_msg="release-readiness schema matches",
        fail_msg=(
            "expected schema 'pi.e2e.release_readiness.v1', got "
            f"{release_readiness.get('schema')!r}"
        ),
        strict=True,
    )
    readiness_correlation_id = str(release_readiness.get("correlation_id", "")).strip()
    require_condition(
        "conformance.release_readiness_correlation_id_nonempty",
        path=release_readiness_path,
        ok=bool(readiness_correlation_id),
        ok_msg="release-readiness correlation_id is set",
        fail_msg="release-readiness correlation_id is empty",
        strict=True,
        remediation="Set correlation_id from summary.json when generating release_readiness_summary.json.",
    )
    require_condition(
        "conformance.release_readiness_correlation_id_matches_summary",
        path=release_readiness_path,
        ok=bool(summary_correlation_id) and readiness_correlation_id == summary_correlation_id,
        ok_msg="release-readiness correlation_id matches summary correlation_id",
        fail_msg=(
            "release-readiness correlation_id mismatch: "
            f"expected {summary_correlation_id!r}, got {readiness_correlation_id!r}"
        ),
        strict=True,
        remediation="Propagate summary correlation_id into release_readiness_summary.json.",
    )

    profiles = release_readiness.get("profiles")
    require_condition(
        "conformance.release_readiness_profiles_object",
        path=release_readiness_path,
        ok=isinstance(profiles, dict),
        ok_msg="profiles object exists",
        fail_msg="profiles must be an object",
        strict=True,
    )
    if isinstance(profiles, dict):
        for profile_name in ("safe", "balanced", "permissive"):
            profile_entry = profiles.get(profile_name)
            require_condition(
                f"conformance.release_readiness.{profile_name}.object",
                path=release_readiness_path,
                ok=isinstance(profile_entry, dict),
                ok_msg=f"{profile_name} profile entry is object",
                fail_msg=f"{profile_name} profile entry missing/invalid",
                strict=True,
            )
            if isinstance(profile_entry, dict):
                for field in (
                    "projected_pass_rate_pct",
                    "blocked_extensions",
                    "recommendation",
                ):
                    require_condition(
                        f"conformance.release_readiness.{profile_name}.{field}",
                        path=release_readiness_path,
                        ok=field in profile_entry,
                        ok_msg=f"{field} present",
                        fail_msg=f"missing {field}",
                        strict=True,
                    )

    workflow_outcomes = release_readiness.get("workflow_outcomes")
    require_condition(
        "conformance.release_readiness.workflow_outcomes_object",
        path=release_readiness_path,
        ok=isinstance(workflow_outcomes, dict),
        ok_msg="workflow_outcomes object exists",
        fail_msg="workflow_outcomes must be an object",
        strict=True,
    )
    if isinstance(workflow_outcomes, dict):
        require_condition(
            "conformance.release_readiness.workflow_outcomes.failing_suites_array",
            path=release_readiness_path,
            ok=isinstance(workflow_outcomes.get("failing_suites"), list),
            ok_msg="failing_suites is an array",
            fail_msg="failing_suites must be an array",
            strict=True,
        )
        require_condition(
            "conformance.release_readiness.workflow_outcomes.failing_unit_targets_array",
            path=release_readiness_path,
            ok=isinstance(workflow_outcomes.get("failing_unit_targets"), list),
            ok_msg="failing_unit_targets is an array",
            fail_msg="failing_unit_targets must be an array",
            strict=True,
        )

    known_risks = release_readiness.get("known_risks")
    require_condition(
        "conformance.release_readiness.known_risks_array",
        path=release_readiness_path,
        ok=isinstance(known_risks, list),
        ok_msg="known_risks is an array",
        fail_msg="known_risks must be an array",
        strict=True,
    )

    remediation = release_readiness.get("remediation")
    require_condition(
        "conformance.release_readiness.remediation_object",
        path=release_readiness_path,
        ok=isinstance(remediation, dict),
        ok_msg="remediation object exists",
        fail_msg="remediation must be an object",
        strict=True,
    )
    if isinstance(remediation, dict):
        require_condition(
            "conformance.release_readiness.remediation.immediate_array",
            path=release_readiness_path,
            ok=isinstance(remediation.get("immediate"), list),
            ok_msg="immediate remediation is an array",
            fail_msg="remediation.immediate must be an array",
            strict=True,
        )
        require_condition(
            "conformance.release_readiness.remediation.policy_guidance_array",
            path=release_readiness_path,
            ok=isinstance(remediation.get("policy_guidance"), list),
            ok_msg="policy_guidance is an array",
            fail_msg="remediation.policy_guidance must be an array",
            strict=True,
        )

    evidence_links = release_readiness.get("evidence")
    require_condition(
        "conformance.release_readiness.evidence_object",
        path=release_readiness_path,
        ok=isinstance(evidence_links, dict),
        ok_msg="evidence object exists",
        fail_msg="evidence must be an object",
        strict=True,
    )
    if isinstance(evidence_links, dict):
        for field in ("summary_json", "evidence_contract_json"):
            require_condition(
                f"conformance.release_readiness.evidence.{field}",
                path=release_readiness_path,
                ok=field in evidence_links,
                ok_msg=f"{field} present",
                fail_msg=f"missing evidence.{field}",
                strict=True,
            )
        require_condition(
            "conformance.release_readiness.evidence.summary_json_path_matches",
            path=release_readiness_path,
            ok=path_matches(evidence_links.get("summary_json"), summary_path),
            ok_msg="evidence.summary_json path matches summary.json",
            fail_msg=(
                "evidence.summary_json does not match summary artifact path "
                f"{summary_path}"
            ),
            strict=True,
        )
        require_condition(
            "conformance.release_readiness.evidence.evidence_contract_json_path_matches",
            path=release_readiness_path,
            ok=path_matches(evidence_links.get("evidence_contract_json"), contract_file),
            ok_msg="evidence.evidence_contract_json path matches evidence_contract.json",
            fail_msg=(
                "evidence.evidence_contract_json does not match contract artifact path "
                f"{contract_file}"
            ),
            strict=True,
        )

release_readiness_md_path = artifact_dir / "release_readiness_summary.md"
readiness_md_exists = require_file(
    "conformance.release_readiness_markdown",
    release_readiness_md_path,
    strict=True,
    description="user-focused release-readiness markdown exists",
)
if readiness_md_exists:
    content = release_readiness_md_path.read_text(encoding="utf-8")
    require_condition(
        "conformance.release_readiness_markdown.known_risks_section",
        path=release_readiness_md_path,
        ok="## Known Risks" in content,
        ok_msg="contains Known Risks section",
        fail_msg="missing '## Known Risks' section",
        strict=True,
    )
    require_condition(
        "conformance.release_readiness_markdown.remediation_section",
        path=release_readiness_md_path,
        ok="## Recommended Remediation" in content,
        ok_msg="contains Recommended Remediation section",
        fail_msg="missing '## Recommended Remediation' section",
        strict=True,
    )

if isinstance(summary, dict):
    release_meta = summary.get("release_readiness")
    require_condition(
        "summary.release_readiness_object",
        path=summary_path,
        ok=isinstance(release_meta, dict),
        ok_msg="summary includes release_readiness metadata",
        fail_msg="summary missing release_readiness metadata",
        strict=True,
    )
    if isinstance(release_meta, dict):
        require_condition(
            "summary.release_readiness.path_matches",
            path=summary_path,
            ok=str(release_meta.get("path")) == str(release_readiness_path),
            ok_msg="summary.release_readiness.path matches artifact path",
            fail_msg=(
                "summary.release_readiness.path does not match "
                f"{release_readiness_path}"
            ),
            strict=True,
        )
        require_condition(
            "summary.release_readiness.correlation_id_matches",
            path=summary_path,
            ok=str(release_meta.get("correlation_id", "")).strip() == summary_correlation_id,
            ok_msg="summary.release_readiness.correlation_id matches run correlation_id",
            fail_msg=(
                "summary.release_readiness.correlation_id mismatch: expected "
                f"{summary_correlation_id!r}, got {release_meta.get('correlation_id')!r}"
            ),
            strict=True,
            remediation="Write correlation_id into summary.release_readiness metadata.",
        )


# 6) Claim-integrity fail-closed checks for benchmark/conformance evidence (bd-3ar8v.1.12)
perf_sli_matrix_path = project_root / "docs" / "perf_sli_matrix.json"
perf_sli_matrix = load_json(
    "claim_integrity.perf_sli_matrix_json",
    perf_sli_matrix_path,
    strict=True,
)
require_keys(
    "claim_integrity.perf_sli_matrix_json",
    perf_sli_matrix,
    perf_sli_matrix_path,
    ["schema", "reporting_contract", "ci_enforcement", "workflow_sli_mapping", "sli_catalog"],
    strict=True,
)

required_fail_closed_conditions = [
    "missing_required_result_field",
    "scenario_without_sli_mapping",
    "sli_without_thresholds",
    "responsiveness_scenario_missing_primary_latency_sli",
    "responsiveness_workflow_missing_primary_latency_sli",
    "missing_or_stale_evidence",
    "missing_absolute_or_relative_values",
    "missing_workload_partition_tag",
    "missing_scenario_metadata",
    "invalid_evidence_class",
    "invalid_confidence_label",
    "microbench_only_claim",
    "global_claim_missing_partition_coverage",
    "unresolved_conflicting_claims",
]

required_scenarios: list[str] = []
required_partition_tags: list[str] = ["matched-state", "realistic"]
BUN_KILLER_MAX_RUST_VS_BUN_RATIO = 0.33
required_realistic_session_shapes: list[str] = [
    "realistic_100k",
    "realistic_200k",
    "realistic_500k",
    "realistic_1m",
    "realistic_5m",
]
required_scenario_metadata_fields: list[str] = [
    "workflow_id",
    "workflow_class",
    "suite_ids",
    "vcr_mode",
    "scenario_owner",
]
allowed_evidence_class: list[str] = ["measured", "inferred"]
allowed_confidence: list[str] = ["high", "medium", "low"]
workflow_sli_mapping: list[dict] = []
sli_threshold_ids: set[str] = set()
scenario_to_sli_ids: dict[str, set[str]] = {}
workflow_to_sli_ids: dict[str, set[str]] = {}
primary_sli_ids: set[str] = set()
responsiveness_workflow_ids: set[str] = set()
responsiveness_markers = (
    "swarm",
    "performance",
    "responsiv",
    "latency",
    "smooth",
    "fast",
)

if isinstance(perf_sli_matrix, dict):
    require_condition(
        "claim_integrity.perf_sli_matrix_schema",
        path=perf_sli_matrix_path,
        ok=str(perf_sli_matrix.get("schema", "")).startswith("pi.perf.sli_ux_matrix."),
        ok_msg="perf SLI matrix schema is versioned",
        fail_msg=(
            "perf SLI matrix schema must start with "
            "'pi.perf.sli_ux_matrix.'"
        ),
        strict=True,
    )

    ci_enforcement = perf_sli_matrix.get("ci_enforcement")
    require_condition(
        "claim_integrity.ci_enforcement_object",
        path=perf_sli_matrix_path,
        ok=isinstance(ci_enforcement, dict),
        ok_msg="ci_enforcement object exists",
        fail_msg="ci_enforcement must be an object",
        strict=True,
    )
    if isinstance(ci_enforcement, dict):
        fail_closed_conditions = ci_enforcement.get("fail_closed_conditions")
        fail_closed_list = (
            fail_closed_conditions
            if isinstance(fail_closed_conditions, list)
            else []
        )
        fail_closed_set = {
            str(value).strip()
            for value in fail_closed_list
            if str(value).strip()
        }
        missing_fail_closed = sorted(
            condition
            for condition in required_fail_closed_conditions
            if condition not in fail_closed_set
        )
        require_condition(
            "claim_integrity.fail_closed_conditions_complete",
            path=perf_sli_matrix_path,
            ok=not missing_fail_closed,
            ok_msg=(
                f"ci_enforcement.fail_closed_conditions includes "
                f"{len(required_fail_closed_conditions)} required IDs"
            ),
            fail_msg=(
                "ci_enforcement.fail_closed_conditions missing IDs: "
                f"{missing_fail_closed}"
            ),
            strict=True,
            remediation=(
                "Restore all required fail_closed_conditions in "
                "docs/perf_sli_matrix.json ci_enforcement."
            ),
        )

        allowed_evidence_class_raw = ci_enforcement.get("allowed_evidence_class")
        if isinstance(allowed_evidence_class_raw, list):
            normalized = [
                str(label).strip()
                for label in allowed_evidence_class_raw
                if str(label).strip()
            ]
            if normalized:
                allowed_evidence_class = normalized
        allowed_confidence_raw = ci_enforcement.get("allowed_confidence")
        if isinstance(allowed_confidence_raw, list):
            normalized = [
                str(label).strip()
                for label in allowed_confidence_raw
                if str(label).strip()
            ]
            if normalized:
                allowed_confidence = normalized

    reporting_contract = perf_sli_matrix.get("reporting_contract")
    require_condition(
        "claim_integrity.reporting_contract_object",
        path=perf_sli_matrix_path,
        ok=isinstance(reporting_contract, dict),
        ok_msg="reporting_contract object exists",
        fail_msg="reporting_contract must be an object",
        strict=True,
    )
    benchmark_partitions = perf_sli_matrix.get("benchmark_partitions")
    require_condition(
        "claim_integrity.benchmark_partitions_object",
        path=perf_sli_matrix_path,
        ok=isinstance(benchmark_partitions, dict),
        ok_msg="benchmark_partitions object exists",
        fail_msg="benchmark_partitions must be an object",
        strict=True,
    )
    if isinstance(benchmark_partitions, dict):
        realistic_shapes_raw = benchmark_partitions.get("realistic_long_session")
        if isinstance(realistic_shapes_raw, list):
            normalized = [
                str(value).strip()
                for value in realistic_shapes_raw
                if str(value).strip()
            ]
            if normalized:
                required_realistic_session_shapes = normalized
    if isinstance(reporting_contract, dict):
        required_scenarios_raw = reporting_contract.get("required_scenarios")
        if isinstance(required_scenarios_raw, list):
            required_scenarios = [
                str(value).strip()
                for value in required_scenarios_raw
                if str(value).strip()
            ]
        required_partition_tags_raw = reporting_contract.get("required_partition_tags")
        if isinstance(required_partition_tags_raw, list):
            normalized = [
                str(value).strip()
                for value in required_partition_tags_raw
                if str(value).strip()
            ]
            if normalized:
                required_partition_tags = normalized
        scenario_metadata_fields_raw = reporting_contract.get(
            "required_scenario_metadata_fields"
        )
        if isinstance(scenario_metadata_fields_raw, list):
            normalized = [
                str(value).strip()
                for value in scenario_metadata_fields_raw
                if str(value).strip()
            ]
            if normalized:
                required_scenario_metadata_fields = normalized

    metric_hierarchy = perf_sli_matrix.get("metric_hierarchy")
    if isinstance(metric_hierarchy, dict):
        primary_raw = metric_hierarchy.get("primary")
        if isinstance(primary_raw, list):
            primary_sli_ids = {
                str(value).strip()
                for value in primary_raw
                if str(value).strip()
            }

    scenario_sli_matrix = perf_sli_matrix.get("scenario_sli_matrix")
    if isinstance(scenario_sli_matrix, list):
        for row in scenario_sli_matrix:
            if not isinstance(row, dict):
                continue
            workflow_id = str(row.get("scenario_id", "")).strip()
            scenario_sli_ids: set[str] = set()
            sli_ids = row.get("sli_ids")
            if isinstance(sli_ids, list):
                scenario_sli_ids = {
                    str(sli_id).strip()
                    for sli_id in sli_ids
                    if str(sli_id).strip()
                }
            if workflow_id:
                scenario_to_sli_ids[workflow_id] = scenario_sli_ids
            user_outcome = str(row.get("user_outcome", "")).strip()
            searchable = f"{workflow_id}\n{user_outcome}".lower()
            if workflow_id and any(marker in searchable for marker in responsiveness_markers):
                responsiveness_workflow_ids.add(workflow_id)

    workflow_sli_mapping_payload = perf_sli_matrix.get("workflow_sli_mapping")
    if isinstance(workflow_sli_mapping_payload, list):
        workflow_sli_mapping = [
            row for row in workflow_sli_mapping_payload if isinstance(row, dict)
        ]

    sli_catalog_payload = perf_sli_matrix.get("sli_catalog")
    if isinstance(sli_catalog_payload, list):
        for row in sli_catalog_payload:
            if not isinstance(row, dict):
                continue
            sli_id = str(row.get("sli_id", "")).strip()
            thresholds = row.get("thresholds")
            if sli_id and isinstance(thresholds, dict) and thresholds:
                sli_threshold_ids.add(sli_id)

mapped_workflow_ids: set[str] = set()
mapped_sli_ids: set[str] = set()
for mapping in workflow_sli_mapping:
    workflow_id = str(mapping.get("workflow_id", "")).strip()
    if workflow_id:
        mapped_workflow_ids.add(workflow_id)
    workflow_sli_ids: set[str] = set()
    sli_ids = mapping.get("sli_ids")
    if isinstance(sli_ids, list):
        for sli_id in sli_ids:
            normalized = str(sli_id).strip()
            if normalized:
                mapped_sli_ids.add(normalized)
                workflow_sli_ids.add(normalized)
    if workflow_id:
        workflow_to_sli_ids[workflow_id] = workflow_sli_ids

missing_scenario_mappings = sorted(
    scenario
    for scenario in required_scenarios
    if scenario not in mapped_workflow_ids
)
require_condition(
    "claim_integrity.scenario_without_sli_mapping",
    path=perf_sli_matrix_path,
    ok=not missing_scenario_mappings,
    ok_msg="every required scenario has workflow_sli_mapping coverage",
    fail_msg=(
        "required scenarios missing workflow_sli_mapping entries: "
        f"{missing_scenario_mappings}"
    ),
    strict=True,
    remediation=(
        "Add missing workflow_id rows to docs/perf_sli_matrix.json "
        "workflow_sli_mapping."
    ),
)

missing_sli_thresholds = sorted(
    sli_id for sli_id in mapped_sli_ids if sli_id not in sli_threshold_ids
)
require_condition(
    "claim_integrity.sli_without_thresholds",
    path=perf_sli_matrix_path,
    ok=not missing_sli_thresholds,
    ok_msg="every mapped SLI has thresholds in sli_catalog",
    fail_msg=f"workflow_sli_mapping references SLI IDs without thresholds: {missing_sli_thresholds}",
    strict=True,
    remediation=(
        "Define thresholds for referenced SLI IDs in "
        "docs/perf_sli_matrix.json sli_catalog."
    ),
)

for workflow_id in required_scenarios:
    if any(marker in workflow_id.lower() for marker in responsiveness_markers):
        responsiveness_workflow_ids.add(workflow_id)

responsiveness_scenarios_missing_primary_latency = sorted(
    workflow_id
    for workflow_id in responsiveness_workflow_ids
    if workflow_id in scenario_to_sli_ids
    and scenario_to_sli_ids.get(workflow_id, set()).isdisjoint(primary_sli_ids)
)
require_condition(
    "claim_integrity.responsiveness_scenario_missing_primary_latency_sli",
    path=perf_sli_matrix_path,
    ok=bool(primary_sli_ids) and not responsiveness_scenarios_missing_primary_latency,
    ok_msg="responsiveness scenario rows retain primary user-visible latency SLI coverage",
    fail_msg=(
        "responsiveness scenario rows missing primary latency SLI coverage: "
        f"{responsiveness_scenarios_missing_primary_latency}"
        if primary_sli_ids
        else "metric_hierarchy.primary does not define primary latency SLI IDs"
    ),
    strict=True,
    remediation=(
        "Add at least one metric_hierarchy.primary SLI to every responsiveness "
        "scenario_sli_matrix row in docs/perf_sli_matrix.json."
    ),
)

responsiveness_missing_primary_latency = sorted(
    workflow_id
    for workflow_id in responsiveness_workflow_ids
    if workflow_id in mapped_workflow_ids
    and workflow_to_sli_ids.get(workflow_id, set()).isdisjoint(primary_sli_ids)
)
require_condition(
    "claim_integrity.responsiveness_workflow_missing_primary_latency_sli",
    path=perf_sli_matrix_path,
    ok=bool(primary_sli_ids) and not responsiveness_missing_primary_latency,
    ok_msg="responsiveness workflows retain primary user-visible latency SLI coverage",
    fail_msg=(
        "responsiveness workflows missing primary latency SLI coverage: "
        f"{responsiveness_missing_primary_latency}"
        if primary_sli_ids
        else "metric_hierarchy.primary does not define primary latency SLI IDs"
    ),
    strict=True,
    remediation=(
        "Add at least one metric_hierarchy.primary SLI to every responsiveness "
        "workflow_sli_mapping row in docs/perf_sli_matrix.json."
    ),
)

claim_integrity_gate_active = (
    claim_integrity_required
    or perf_extension_stratification_path is not None
    or perf_baseline_confidence_path is not None
)
add_check(
    "claim_integrity.gate_active",
    perf_sli_matrix_path,
    True,
    (
        "claim-integrity artifact validation enabled"
        if claim_integrity_gate_active
        else "claim-integrity artifact validation skipped (no perf evidence paths configured)"
    ),
)

evidence_missing_or_stale_reasons: list[str] = []
missing_required_result_field_reasons: list[str] = []
missing_workload_partition_tag_reasons: list[str] = []
missing_scenario_metadata_reasons: list[str] = []
missing_realistic_session_shape_reasons: list[str] = []
invalid_evidence_class_reasons: list[str] = []
invalid_confidence_label_reasons: list[str] = []
missing_absolute_or_relative_reasons: list[str] = []
microbench_only_claim_reasons: list[str] = []
global_claim_partition_reasons: list[str] = []
unresolved_conflicting_claim_reasons: list[str] = []
scenario_partition_coverage: dict[str, set[str]] = {}
scenario_class_by_id: dict[str, str] = {}
scenario_partition_confidence_labels: dict[tuple[str, str], set[str]] = {}
observed_realistic_session_shapes: set[str] = set()
phase1_realistic_session_shapes_observed: set[str] = set()
missing_realistic_session_shapes: list[str] = []
realistic_session_shape_coverage_source = "baseline_confidence"
scenario_cell_status_payload: dict | None = None
scenario_cell_status_json_path: Path | None = None
scenario_cell_status_markdown_path: Path | None = None
adjudication_matrix_payload: dict | None = None
adjudication_matrix_json_path: Path | None = None
adjudication_matrix_markdown_path: Path | None = None
franken_node_claim_gate_status_payload: dict | None = None
franken_node_claim_gate_status_json_path: Path | None = None
franken_node_kernel_boundary_drift_report_payload: dict | None = None
franken_node_kernel_boundary_drift_report_json_path: Path | None = None
kernel_boundary_drift_report_filename = "franken_node_kernel_boundary_drift_report.json"

expected_claim_correlation_id = summary_correlation_id or environment_correlation_id
freshness_cutoff = datetime.now(timezone.utc) - timedelta(days=7)

def validate_generated_at_freshness(
    *,
    payload: dict,
    payload_path: Path,
    check_id: str,
    label: str,
) -> None:
    generated_at_value = payload.get("generated_at")
    generated_at = parse_iso8601_timestamp(generated_at_value)
    is_fresh = generated_at is not None and generated_at >= freshness_cutoff
    require_condition(
        check_id,
        path=payload_path,
        ok=is_fresh,
        ok_msg=f"{label} generated_at is fresh ({generated_at_value!r})",
        fail_msg=(
            f"{label} generated_at is missing/invalid/stale: {generated_at_value!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Regenerate perf evidence in the current CI run via "
            "./scripts/perf/orchestrate.sh --profile ci."
        ),
    )
    if not is_fresh and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            f"{label} generated_at stale/invalid: {generated_at_value!r}"
        )


def parse_session_messages_value(raw: object) -> int | None:
    if isinstance(raw, bool):
        return None
    if isinstance(raw, (int, float)):
        value = int(raw)
        return value if value > 0 else None
    text = str(raw or "").strip().lower().replace(",", "")
    if not text:
        return None
    suffix_match = re.search(r"(\d+)\s*([km]?)$", text)
    if not suffix_match:
        return None
    magnitude = int(suffix_match.group(1))
    suffix = suffix_match.group(2)
    if suffix == "k":
        magnitude *= 1_000
    elif suffix == "m":
        magnitude *= 1_000_000
    return magnitude if magnitude > 0 else None


def parse_positive_metric_value(raw: object) -> float | None:
    if isinstance(raw, bool):
        return None
    if isinstance(raw, (int, float)):
        value = float(raw)
    else:
        text = str(raw or "").strip().replace(",", "")
        if not text:
            return None
        try:
            value = float(text)
        except ValueError:
            return None
    return value if math.isfinite(value) and value > 0 else None


def metric_value_missing(raw: object) -> bool:
    if raw is None:
        return True
    if isinstance(raw, str):
        return not raw.strip()
    return False


def confidence_rank(label: object) -> int:
    normalized = str(label or "").strip().lower()
    if normalized == "high":
        return 3
    if normalized == "medium":
        return 2
    if normalized == "low":
        return 1
    return 0


def normalize_confidence_label(label: object) -> str:
    normalized = str(label or "").strip().lower()
    return normalized if normalized in {"high", "medium", "low"} else "unknown"


def highest_confidence_label(labels: list[str]) -> str:
    ranked = sorted(
        (normalize_confidence_label(label) for label in labels),
        key=confidence_rank,
        reverse=True,
    )
    return ranked[0] if ranked else "unknown"


def normalize_claim_outcome(raw: object) -> str:
    normalized = str(raw or "").strip().lower()
    if normalized in {"pass", "passed", "ok", "success", "valid"}:
        return "pass"
    if normalized in {"fail", "failed", "error", "invalid"}:
        return "fail"
    if normalized in {"warn", "warning"}:
        return "warn"
    if normalized in {"missing", "none", "no_data", "unknown", "n/a"}:
        return "missing"
    return "unknown"


def normalize_bool_outcome(raw: object) -> str:
    if isinstance(raw, bool):
        return "pass" if raw else "fail"
    return normalize_claim_outcome(raw)


def build_artifact_claim_metadata(
    *,
    source: str,
    payload: dict | None,
    path: Path | None,
) -> dict:
    generated_at_value = payload.get("generated_at") if isinstance(payload, dict) else None
    generated_at = parse_iso8601_timestamp(generated_at_value)
    fresh = generated_at is not None and generated_at >= freshness_cutoff
    correlation_id = (
        str(payload.get("correlation_id", "")).strip()
        if isinstance(payload, dict)
        else ""
    )
    correlation_matches = (
        bool(expected_claim_correlation_id)
        and bool(correlation_id)
        and correlation_id == expected_claim_correlation_id
    )
    lineage_payload = payload.get("lineage") if isinstance(payload, dict) else None
    if not isinstance(lineage_payload, dict):
        lineage_payload = {}
    return {
        "source": source,
        "schema": payload.get("schema") if isinstance(payload, dict) else None,
        "path": str(path) if path is not None else None,
        "generated_at": generated_at_value,
        "fresh": fresh,
        "correlation_id": correlation_id,
        "correlation_matches": correlation_matches,
        "lineage": lineage_payload,
    }


def parse_shape_to_session_messages(shape: str) -> int | None:
    text = str(shape or "").strip().lower()
    if not text:
        return None
    candidate = text.split("_")[-1]
    return parse_session_messages_value(candidate)


required_realistic_shape_message_counts: dict[int, str] = {}
for shape in required_realistic_session_shapes:
    parsed_count = parse_shape_to_session_messages(shape)
    if parsed_count is not None:
        required_realistic_shape_message_counts[parsed_count] = shape


extension_stratification = None
if perf_extension_stratification_path is None:
    if claim_integrity_gate_active:
        add_check(
            "claim_integrity.extension_stratification_path_configured",
            perf_sli_matrix_path,
            False,
            "PERF_EXTENSION_STRATIFICATION_JSON/PERF_EVIDENCE_DIR not configured",
        )
        record_issue(
            "claim_integrity.extension_stratification_path_configured",
            "missing extension stratification path for claim-integrity validation",
            strict=claim_integrity_required,
            remediation=(
                "Set PERF_EXTENSION_STRATIFICATION_JSON (or PERF_EVIDENCE_DIR) to "
                "extension_benchmark_stratification.json before run_all validation."
            ),
        )
        evidence_missing_or_stale_reasons.append(
            "missing extension_benchmark_stratification.json path"
        )
else:
    extension_stratification = load_json(
        "claim_integrity.extension_stratification_json",
        perf_extension_stratification_path,
        strict=claim_integrity_required,
    )
    if extension_stratification is None and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            f"missing/invalid extension stratification artifact at {perf_extension_stratification_path}"
        )

baseline_confidence = None
if perf_baseline_confidence_path is None:
    if claim_integrity_gate_active:
        add_check(
            "claim_integrity.baseline_confidence_path_configured",
            perf_sli_matrix_path,
            False,
            "PERF_BASELINE_CONFIDENCE_JSON/PERF_EVIDENCE_DIR not configured",
        )
        record_issue(
            "claim_integrity.baseline_confidence_path_configured",
            "missing baseline variance confidence path for claim-integrity validation",
            strict=claim_integrity_required,
            remediation=(
                "Set PERF_BASELINE_CONFIDENCE_JSON (or PERF_EVIDENCE_DIR) to "
                "baseline_variance_confidence.json before run_all validation."
            ),
        )
        evidence_missing_or_stale_reasons.append(
            "missing baseline_variance_confidence.json path"
        )
else:
    baseline_confidence = load_json(
        "claim_integrity.baseline_confidence_json",
        perf_baseline_confidence_path,
        strict=claim_integrity_required,
    )
    if baseline_confidence is None and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            f"missing/invalid baseline confidence artifact at {perf_baseline_confidence_path}"
        )

phase1_matrix_validation = None
if perf_phase1_matrix_validation_path is None:
    if claim_integrity_gate_active:
        add_check(
            "claim_integrity.phase1_matrix_validation_path_configured",
            perf_sli_matrix_path,
            False,
            "PERF_PHASE1_MATRIX_VALIDATION_JSON/PERF_EVIDENCE_DIR not configured",
        )
        record_issue(
            "claim_integrity.phase1_matrix_validation_path_configured",
            "missing phase-1 matrix validation path for claim-integrity realistic tier checks",
            strict=claim_integrity_required,
            remediation=(
                "Set PERF_PHASE1_MATRIX_VALIDATION_JSON (or PERF_EVIDENCE_DIR) to "
                "phase1_matrix_validation.json before run_all validation."
            ),
        )
        evidence_missing_or_stale_reasons.append(
            "missing phase1_matrix_validation.json path"
        )
else:
    phase1_matrix_validation = load_json(
        "claim_integrity.phase1_matrix_validation_json",
        perf_phase1_matrix_validation_path,
        strict=claim_integrity_required,
    )
    if phase1_matrix_validation is None and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "missing/invalid phase-1 matrix validation artifact at "
            f"{perf_phase1_matrix_validation_path}"
        )

parameter_sweeps = None
if perf_parameter_sweeps_path is None:
    if claim_integrity_gate_active:
        add_check(
            "claim_integrity.parameter_sweeps_path_configured",
            perf_sli_matrix_path,
            False,
            "PERF_PARAMETER_SWEEPS_JSON/PERF_EVIDENCE_DIR not configured",
        )
        record_issue(
            "claim_integrity.parameter_sweeps_path_configured",
            "missing parameter_sweeps path for claim-integrity validation",
            strict=claim_integrity_required,
            remediation=(
                "Set PERF_PARAMETER_SWEEPS_JSON (or PERF_EVIDENCE_DIR) to "
                "parameter_sweeps.json before run_all validation."
            ),
        )
        evidence_missing_or_stale_reasons.append("missing parameter_sweeps.json path")
else:
    parameter_sweeps = load_json(
        "claim_integrity.parameter_sweeps_json",
        perf_parameter_sweeps_path,
        strict=claim_integrity_required,
    )
    if parameter_sweeps is None and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            f"missing/invalid parameter_sweeps artifact at {perf_parameter_sweeps_path}"
        )

if isinstance(extension_stratification, dict) and perf_extension_stratification_path is not None:
    require_keys(
        "claim_integrity.extension_stratification_json",
        extension_stratification,
        perf_extension_stratification_path,
        ["schema", "generated_at", "run_id", "correlation_id", "layers", "claim_integrity"],
        strict=claim_integrity_required,
    )
    require_condition(
        "claim_integrity.extension_stratification_schema",
        path=perf_extension_stratification_path,
        ok=extension_stratification.get("schema") == "pi.perf.extension_benchmark_stratification.v1",
        ok_msg="extension stratification schema matches",
        fail_msg=(
            "extension stratification schema mismatch: expected "
            "'pi.perf.extension_benchmark_stratification.v1'"
        ),
        strict=claim_integrity_required,
    )
    validate_generated_at_freshness(
        payload=extension_stratification,
        payload_path=perf_extension_stratification_path,
        check_id="claim_integrity.extension_stratification_generated_at_fresh",
        label="extension stratification",
    )
    strat_correlation_id = str(extension_stratification.get("correlation_id", "")).strip()
    correlation_ok = (
        bool(strat_correlation_id)
        and bool(expected_claim_correlation_id)
        and strat_correlation_id == expected_claim_correlation_id
    )
    require_condition(
        "claim_integrity.extension_stratification_correlation_matches_run",
        path=perf_extension_stratification_path,
        ok=correlation_ok,
        ok_msg="extension stratification correlation_id matches run summary/environment",
        fail_msg=(
            "extension stratification correlation_id mismatch: expected "
            f"{expected_claim_correlation_id!r}, got {strat_correlation_id!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Use a shared CI_CORRELATION_ID for perf orchestrator and "
            "scripts/e2e/run_all.sh."
        ),
    )
    if not correlation_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "extension stratification correlation_id mismatch with run artifacts"
        )

    layers = extension_stratification.get("layers")
    layers_list = layers if isinstance(layers, list) else []
    layers_by_id: dict[str, dict] = {}
    for layer in layers_list:
        if not isinstance(layer, dict):
            continue
        layer_id = str(layer.get("layer_id", "")).strip()
        if layer_id:
            layers_by_id[layer_id] = layer
        evidence_state = str(layer.get("evidence_state", "")).strip()
        if evidence_state and evidence_state not in allowed_evidence_class:
            invalid_evidence_class_reasons.append(
                f"layer {layer_id or '<unknown>'} evidence_state={evidence_state!r}"
            )
        layer_confidence = str(layer.get("confidence", "")).strip()
        if layer_confidence and layer_confidence not in allowed_confidence:
            invalid_confidence_label_reasons.append(
                f"layer {layer_id or '<unknown>'} confidence={layer_confidence!r}"
            )

    claim_integrity_payload = extension_stratification.get("claim_integrity")
    cherry_pick_guard = (
        claim_integrity_payload.get("cherry_pick_guard", {})
        if isinstance(claim_integrity_payload, dict)
        else {}
    )
    invalidity_reasons = (
        cherry_pick_guard.get("invalidity_reasons", [])
        if isinstance(cherry_pick_guard, dict)
        else []
    )
    invalidity_set = {
        str(reason).strip()
        for reason in invalidity_reasons
        if str(reason).strip()
    }
    global_claim_valid = (
        cherry_pick_guard.get("global_claim_valid")
        if isinstance(cherry_pick_guard, dict)
        else None
    )
    if "microbench_only_claim" in invalidity_set:
        microbench_only_claim_reasons.append(
            "extension stratification invalidity_reasons includes microbench_only_claim"
        )
    if global_claim_valid is not True:
        microbench_only_claim_reasons.append(
            f"cherry_pick_guard.global_claim_valid must be true (got {global_claim_valid!r})"
        )

    layer_coverage = (
        cherry_pick_guard.get("layer_coverage", {})
        if isinstance(cherry_pick_guard, dict)
        else {}
    )
    required_layers = [
        "cold_load_init",
        "per_call_dispatch_micro",
        "full_e2e_long_session",
    ]
    for layer_id in required_layers:
        covered = (
            bool(layer_coverage.get(layer_id))
            if isinstance(layer_coverage, dict)
            else False
        )
        if not covered:
            missing_absolute_or_relative_reasons.append(
                f"layer_coverage[{layer_id}] is missing/false"
            )
        layer_payload = layers_by_id.get(layer_id, {})
        absolute_metrics = (
            layer_payload.get("absolute_metrics", {})
            if isinstance(layer_payload, dict)
            else {}
        )
        relative_metrics = (
            layer_payload.get("relative_metrics", {})
            if isinstance(layer_payload, dict)
            else {}
        )
        absolute_value = absolute_metrics.get("value")
        node_ratio = relative_metrics.get("rust_vs_node_ratio")
        bun_ratio = relative_metrics.get("rust_vs_bun_ratio")
        absolute_metric_value = parse_positive_metric_value(absolute_value)
        node_ratio_value = parse_positive_metric_value(node_ratio)
        bun_ratio_value = parse_positive_metric_value(bun_ratio)
        if (
            absolute_metric_value is None
            or node_ratio_value is None
            or bun_ratio_value is None
        ):
            missing_absolute_or_relative_reasons.append(
                f"layer {layer_id} missing absolute/relative metrics"
            )
        elif (
            layer_id == "full_e2e_long_session"
            and bun_ratio_value > BUN_KILLER_MAX_RUST_VS_BUN_RATIO
        ):
            missing_absolute_or_relative_reasons.append(
                "layer full_e2e_long_session rust_vs_bun_ratio "
                f"{bun_ratio_value:.6g} exceeds Bun-killer release gate "
                f"<= {BUN_KILLER_MAX_RUST_VS_BUN_RATIO:.2f}"
            )

    partition_coverage = (
        claim_integrity_payload.get("partition_coverage", {})
        if isinstance(claim_integrity_payload, dict)
        else {}
    )
    missing_partition_tags = [
        tag
        for tag in required_partition_tags
        if not (
            isinstance(partition_coverage, dict)
            and partition_coverage.get(tag) is True
        )
    ]
    if "global_claim_missing_partition_coverage" in invalidity_set:
        global_claim_partition_reasons.append(
            "extension stratification invalidity_reasons includes global_claim_missing_partition_coverage"
        )
    if missing_partition_tags:
        global_claim_partition_reasons.append(
            f"claim_integrity.partition_coverage missing required tags: {missing_partition_tags}"
        )

if isinstance(baseline_confidence, dict) and perf_baseline_confidence_path is not None:
    require_keys(
        "claim_integrity.baseline_confidence_json",
        baseline_confidence,
        perf_baseline_confidence_path,
        ["schema", "generated_at", "run_id", "correlation_id", "records", "summary"],
        strict=claim_integrity_required,
    )
    require_condition(
        "claim_integrity.baseline_confidence_schema",
        path=perf_baseline_confidence_path,
        ok=baseline_confidence.get("schema") == "pi.perf.baseline_variance_confidence.v1",
        ok_msg="baseline confidence schema matches",
        fail_msg=(
            "baseline confidence schema mismatch: expected "
            "'pi.perf.baseline_variance_confidence.v1'"
        ),
        strict=claim_integrity_required,
    )
    validate_generated_at_freshness(
        payload=baseline_confidence,
        payload_path=perf_baseline_confidence_path,
        check_id="claim_integrity.baseline_confidence_generated_at_fresh",
        label="baseline confidence",
    )
    baseline_correlation_id = str(baseline_confidence.get("correlation_id", "")).strip()
    baseline_correlation_ok = (
        bool(baseline_correlation_id)
        and bool(expected_claim_correlation_id)
        and baseline_correlation_id == expected_claim_correlation_id
    )
    require_condition(
        "claim_integrity.baseline_confidence_correlation_matches_run",
        path=perf_baseline_confidence_path,
        ok=baseline_correlation_ok,
        ok_msg="baseline confidence correlation_id matches run summary/environment",
        fail_msg=(
            "baseline confidence correlation_id mismatch: expected "
            f"{expected_claim_correlation_id!r}, got {baseline_correlation_id!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Use a shared CI_CORRELATION_ID for perf orchestrator and "
            "scripts/e2e/run_all.sh."
        ),
    )
    if not baseline_correlation_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "baseline confidence correlation_id mismatch with run artifacts"
        )

    records = baseline_confidence.get("records")
    records_list = records if isinstance(records, list) else []
    require_condition(
        "claim_integrity.baseline_confidence_records_non_empty",
        path=perf_baseline_confidence_path,
        ok=len(records_list) > 0,
        ok_msg=f"baseline confidence has {len(records_list)} record(s)",
        fail_msg="baseline confidence records must be a non-empty array",
        strict=claim_integrity_required,
    )

    minimal_required_fields = [
        "run_id",
        "correlation_id",
        "scenario_id",
        "workload_partition",
        "scenario_metadata",
        "sli_id",
        "confidence",
    ]
    for index, record in enumerate(records_list):
        if not isinstance(record, dict):
            missing_required_result_field_reasons.append(
                f"records[{index}] must be object"
            )
            continue

        missing_fields = [
            field for field in minimal_required_fields if field not in record
        ]
        if missing_fields:
            missing_required_result_field_reasons.append(
                f"records[{index}] missing fields: {missing_fields}"
            )

        scenario_id = str(record.get("scenario_id", "")).strip()
        if not scenario_id:
            missing_required_result_field_reasons.append(
                f"records[{index}] missing non-empty scenario_id"
            )
        elif required_scenarios and scenario_id not in required_scenarios:
            missing_scenario_metadata_reasons.append(
                f"records[{index}] scenario_id {scenario_id!r} not in required_scenarios"
            )

        workload_partition = str(record.get("workload_partition", "")).strip()
        if not workload_partition:
            missing_workload_partition_tag_reasons.append(
                f"records[{index}] missing workload_partition"
            )
        elif workload_partition not in required_partition_tags:
            missing_workload_partition_tag_reasons.append(
                f"records[{index}] workload_partition {workload_partition!r} not in {required_partition_tags}"
            )

        metadata = record.get("scenario_metadata")
        realistic_session_shape = ""
        if not isinstance(metadata, dict):
            missing_scenario_metadata_reasons.append(
                f"records[{index}] scenario_metadata must be object"
            )
        else:
            workflow_class = str(metadata.get("workflow_class", "")).strip()
            if scenario_id and workflow_class:
                scenario_class_by_id[scenario_id] = workflow_class
            missing_metadata_fields = [
                field
                for field in required_scenario_metadata_fields
                if field not in metadata
            ]
            if missing_metadata_fields:
                missing_scenario_metadata_reasons.append(
                    f"records[{index}] scenario_metadata missing {missing_metadata_fields}"
                )
            if workload_partition == "realistic":
                realistic_session_shape = str(
                    metadata.get("realistic_session_shape", "")
                ).strip()
                if (
                    realistic_session_shape
                    and required_realistic_session_shapes
                    and realistic_session_shape not in required_realistic_session_shapes
                ):
                    missing_realistic_session_shape_reasons.append(
                        f"records[{index}] scenario_metadata.realistic_session_shape "
                        f"{realistic_session_shape!r} not in {required_realistic_session_shapes}"
                    )
                if (
                    realistic_session_shape
                    and required_realistic_session_shapes
                    and realistic_session_shape in required_realistic_session_shapes
                ):
                    observed_realistic_session_shapes.add(realistic_session_shape)

        confidence_label = str(record.get("confidence", "")).strip()
        if confidence_label and confidence_label not in allowed_confidence:
            invalid_confidence_label_reasons.append(
                f"records[{index}] confidence={confidence_label!r}"
            )
        if scenario_id and workload_partition:
            normalized_confidence = normalize_confidence_label(confidence_label)
            if normalized_confidence != "unknown":
                scenario_partition_confidence_labels.setdefault(
                    (scenario_id, workload_partition), set()
                ).add(normalized_confidence)

        evidence_state = str(record.get("evidence_state", "")).strip()
        if evidence_state and evidence_state not in allowed_evidence_class:
            invalid_evidence_class_reasons.append(
                f"records[{index}] evidence_state={evidence_state!r}"
            )

        if scenario_id and workload_partition:
            scenario_partition_coverage.setdefault(scenario_id, set()).add(workload_partition)

    for scenario in required_scenarios:
        present_partitions = scenario_partition_coverage.get(scenario, set())
        missing_tags = [
            tag for tag in required_partition_tags if tag not in present_partitions
        ]
        if missing_tags:
            global_claim_partition_reasons.append(
                f"scenario {scenario} missing partition coverage: {missing_tags}"
            )

if isinstance(phase1_matrix_validation, dict) and perf_phase1_matrix_validation_path is not None:
    require_keys(
        "claim_integrity.phase1_matrix_validation_json",
        phase1_matrix_validation,
        perf_phase1_matrix_validation_path,
        [
            "schema",
            "generated_at",
            "run_id",
            "correlation_id",
            "matrix_requirements",
            "matrix_cells",
            "stage_summary",
            "primary_outcomes",
        ],
        strict=claim_integrity_required,
    )
    require_condition(
        "claim_integrity.phase1_matrix_validation_schema",
        path=perf_phase1_matrix_validation_path,
        ok=phase1_matrix_validation.get("schema") == "pi.perf.phase1_matrix_validation.v1",
        ok_msg="phase-1 matrix validation schema matches",
        fail_msg=(
            "phase-1 matrix validation schema mismatch: expected "
            "'pi.perf.phase1_matrix_validation.v1'"
        ),
        strict=claim_integrity_required,
    )
    validate_generated_at_freshness(
        payload=phase1_matrix_validation,
        payload_path=perf_phase1_matrix_validation_path,
        check_id="claim_integrity.phase1_matrix_validation_generated_at_fresh",
        label="phase-1 matrix validation",
    )
    phase1_correlation_id = str(phase1_matrix_validation.get("correlation_id", "")).strip()
    phase1_correlation_ok = (
        bool(phase1_correlation_id)
        and bool(expected_claim_correlation_id)
        and phase1_correlation_id == expected_claim_correlation_id
    )
    require_condition(
        "claim_integrity.phase1_matrix_correlation_matches_run",
        path=perf_phase1_matrix_validation_path,
        ok=phase1_correlation_ok,
        ok_msg="phase-1 matrix validation correlation_id matches run summary/environment",
        fail_msg=(
            "phase-1 matrix validation correlation_id mismatch: expected "
            f"{expected_claim_correlation_id!r}, got {phase1_correlation_id!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Use a shared CI_CORRELATION_ID for perf orchestrator and "
            "scripts/e2e/run_all.sh."
        ),
    )
    if not phase1_correlation_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation correlation_id mismatch with run artifacts"
        )

    primary_outcomes = phase1_matrix_validation.get("primary_outcomes")
    primary_outcomes_obj = primary_outcomes if isinstance(primary_outcomes, dict) else None
    require_condition(
        "claim_integrity.phase1_matrix_primary_outcomes_object",
        path=perf_phase1_matrix_validation_path,
        ok=primary_outcomes_obj is not None,
        ok_msg="phase-1 matrix validation primary_outcomes object present",
        fail_msg="phase-1 matrix validation missing primary_outcomes object",
        strict=claim_integrity_required,
        remediation=(
            "Update scripts/perf/orchestrate.sh to emit primary_outcomes with "
            "status, wall_clock_ms, rust_vs_node_ratio, rust_vs_bun_ratio, "
            "and ordering_policy."
        ),
    )

    required_primary_outcome_fields = [
        "status",
        "wall_clock_ms",
        "rust_vs_node_ratio",
        "rust_vs_bun_ratio",
        "ordering_policy",
    ]
    missing_primary_outcome_fields = [
        field
        for field in required_primary_outcome_fields
        if not (
            isinstance(primary_outcomes_obj, dict)
            and field in primary_outcomes_obj
        )
    ]
    require_condition(
        "claim_integrity.phase1_matrix_primary_outcomes_required_fields",
        path=perf_phase1_matrix_validation_path,
        ok=not missing_primary_outcome_fields,
        ok_msg="phase-1 matrix validation primary_outcomes required fields present",
        fail_msg=(
            "phase-1 matrix validation primary_outcomes missing fields: "
            f"{missing_primary_outcome_fields}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit all primary_outcomes fields (status, wall_clock_ms, "
            "rust_vs_node_ratio, rust_vs_bun_ratio, ordering_policy) in "
            "scripts/perf/orchestrate.sh."
        ),
    )
    if missing_primary_outcome_fields and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation primary_outcomes missing required fields: "
            f"{missing_primary_outcome_fields}"
        )

    primary_outcome_status = str(
        (
            primary_outcomes_obj.get("status")
            if isinstance(primary_outcomes_obj, dict)
            else ""
        )
        or ""
    ).strip().lower()
    primary_outcome_status_ok = primary_outcome_status in {"pass", "fail"}
    require_condition(
        "claim_integrity.phase1_matrix_primary_outcomes_status_valid",
        path=perf_phase1_matrix_validation_path,
        ok=primary_outcome_status_ok,
        ok_msg="phase-1 matrix validation primary_outcomes.status is pass/fail",
        fail_msg=(
            "phase-1 matrix validation primary_outcomes.status invalid: "
            f"{primary_outcome_status!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set phase1_matrix_validation.primary_outcomes.status to "
            "'pass' or 'fail' in scripts/perf/orchestrate.sh."
        ),
    )
    if not primary_outcome_status_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation primary_outcomes.status must be "
            "'pass' or 'fail'"
        )

    invalid_primary_metric_fields: list[str] = []
    for field in ("wall_clock_ms", "rust_vs_node_ratio", "rust_vs_bun_ratio"):
        metric_value = parse_positive_metric_value(
            primary_outcomes_obj.get(field) if isinstance(primary_outcomes_obj, dict) else None
        )
        if metric_value is None:
            invalid_primary_metric_fields.append(field)
    require_condition(
        "claim_integrity.phase1_matrix_primary_outcomes_metrics_present",
        path=perf_phase1_matrix_validation_path,
        ok=not invalid_primary_metric_fields,
        ok_msg="phase-1 matrix validation primary outcomes include positive wall-clock and ratio metrics",
        fail_msg=(
            "phase-1 matrix validation primary outcomes missing/invalid metrics: "
            f"{invalid_primary_metric_fields}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Ensure scripts/perf/orchestrate.sh populates positive primary outcomes "
            "for wall_clock_ms, rust_vs_node_ratio, and rust_vs_bun_ratio."
        ),
    )
    if invalid_primary_metric_fields and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation primary outcomes missing/invalid metrics: "
            f"{invalid_primary_metric_fields}"
        )

    primary_bun_ratio = parse_positive_metric_value(
        primary_outcomes_obj.get("rust_vs_bun_ratio")
        if isinstance(primary_outcomes_obj, dict)
        else None
    )
    bun_killer_primary_ratio_ok = (
        primary_bun_ratio is not None
        and primary_bun_ratio <= BUN_KILLER_MAX_RUST_VS_BUN_RATIO
    )
    require_condition(
        "claim_integrity.phase1_matrix_primary_outcomes_bun_killer_threshold",
        path=perf_phase1_matrix_validation_path,
        ok=bun_killer_primary_ratio_ok,
        ok_msg=(
            "phase-1 matrix validation rust_vs_bun_ratio satisfies "
            "Bun-killer release gate"
        ),
        fail_msg=(
            "phase-1 matrix validation rust_vs_bun_ratio exceeds Bun-killer "
            f"release gate <= {BUN_KILLER_MAX_RUST_VS_BUN_RATIO:.2f} "
            f"(got {primary_bun_ratio!r})"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Reduce phase1_matrix_validation.primary_outcomes.rust_vs_bun_ratio "
            f"to <= {BUN_KILLER_MAX_RUST_VS_BUN_RATIO:.2f} before release promotion."
        ),
    )
    if not bun_killer_primary_ratio_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation rust_vs_bun_ratio exceeds Bun-killer "
            "release gate"
        )

    ordering_policy = str(
        (
            primary_outcomes_obj.get("ordering_policy")
            if isinstance(primary_outcomes_obj, dict)
            else ""
        )
        or ""
    ).strip()
    ordering_policy_ok = ordering_policy == "primary_e2e_before_microbench"
    require_condition(
        "claim_integrity.phase1_matrix_primary_outcomes_ordering_policy",
        path=perf_phase1_matrix_validation_path,
        ok=ordering_policy_ok,
        ok_msg="phase-1 matrix validation ordering policy prioritizes primary E2E outcomes",
        fail_msg=(
            "phase-1 matrix validation ordering_policy mismatch: expected "
            "'primary_e2e_before_microbench', got "
            f"{ordering_policy!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set phase1_matrix_validation.primary_outcomes.ordering_policy to "
            "'primary_e2e_before_microbench' in scripts/perf/orchestrate.sh."
        ),
    )
    if not ordering_policy_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation ordering_policy must be "
            "'primary_e2e_before_microbench'"
        )

    regression_guards = phase1_matrix_validation.get("regression_guards")
    regression_guards_obj = (
        regression_guards if isinstance(regression_guards, dict) else None
    )
    require_condition(
        "claim_integrity.phase1_matrix_regression_guards_object",
        path=perf_phase1_matrix_validation_path,
        ok=regression_guards_obj is not None,
        ok_msg="phase-1 matrix validation regression_guards object present",
        fail_msg="phase-1 matrix validation missing regression_guards object",
        strict=claim_integrity_required,
        remediation=(
            "Update scripts/perf/orchestrate.sh to emit regression_guards with "
            "memory/correctness/security statuses and failure_or_gap_reasons."
        ),
    )

    required_regression_guard_fields = [
        "memory",
        "correctness",
        "security",
        "failure_or_gap_reasons",
    ]
    missing_regression_guard_fields = [
        field
        for field in required_regression_guard_fields
        if not (
            isinstance(regression_guards_obj, dict) and field in regression_guards_obj
        )
    ]
    require_condition(
        "claim_integrity.phase1_matrix_regression_guards_required_fields",
        path=perf_phase1_matrix_validation_path,
        ok=not missing_regression_guard_fields,
        ok_msg="phase-1 matrix validation regression_guards required fields present",
        fail_msg=(
            "phase-1 matrix validation regression_guards missing fields: "
            f"{missing_regression_guard_fields}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit regression_guards.memory, regression_guards.correctness, "
            "regression_guards.security, and regression_guards.failure_or_gap_reasons "
            "in scripts/perf/orchestrate.sh."
        ),
    )
    if missing_regression_guard_fields and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation regression_guards missing required fields: "
            f"{missing_regression_guard_fields}"
        )

    failure_or_gap_reasons_raw = (
        regression_guards_obj.get("failure_or_gap_reasons")
        if isinstance(regression_guards_obj, dict)
        else None
    )
    normalized_failure_or_gap_reasons: list[str] = []
    invalid_failure_or_gap_reasons: list[str] = []
    duplicate_failure_or_gap_reasons: list[str] = []
    seen_failure_or_gap_reasons: set[str] = set()
    if isinstance(failure_or_gap_reasons_raw, list):
        for index, raw_reason in enumerate(failure_or_gap_reasons_raw):
            if not isinstance(raw_reason, str):
                invalid_failure_or_gap_reasons.append(
                    f"index {index} non-string value {raw_reason!r}"
                )
                continue
            normalized_reason = raw_reason.strip().lower()
            if not normalized_reason:
                invalid_failure_or_gap_reasons.append(
                    f"index {index} empty reason token"
                )
                continue
            normalized_failure_or_gap_reasons.append(normalized_reason)
            if normalized_reason in seen_failure_or_gap_reasons:
                duplicate_failure_or_gap_reasons.append(normalized_reason)
            else:
                seen_failure_or_gap_reasons.add(normalized_reason)
    elif regression_guards_obj is not None:
        invalid_failure_or_gap_reasons.append(
            f"failure_or_gap_reasons is not an array: {type(failure_or_gap_reasons_raw).__name__}"
        )
    duplicate_failure_or_gap_reasons = sorted(set(duplicate_failure_or_gap_reasons))
    regression_guard_reason_format_ok = (
        not invalid_failure_or_gap_reasons and not duplicate_failure_or_gap_reasons
    )
    require_condition(
        "claim_integrity.phase1_matrix_regression_guard_reasons_format",
        path=perf_phase1_matrix_validation_path,
        ok=regression_guard_reason_format_ok,
        ok_msg="phase-1 matrix regression guard reasons are non-empty unique strings",
        fail_msg=(
            "phase-1 matrix regression guard reasons invalid: "
            f"invalid={invalid_failure_or_gap_reasons}, "
            f"duplicates={duplicate_failure_or_gap_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set phase1_matrix_validation.regression_guards.failure_or_gap_reasons "
            "to a unique array of non-empty strings in scripts/perf/orchestrate.sh."
        ),
    )
    if not regression_guard_reason_format_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation regression_guards.failure_or_gap_reasons "
            "must be a unique array of non-empty strings"
        )

    normalized_failure_or_gap_reason_set = set(normalized_failure_or_gap_reasons)
    known_regression_guard_reasons = {
        f"{guard_name}_regression"
        for guard_name in ("memory", "correctness", "security")
    } | {
        f"{guard_name}_regression_unverified"
        for guard_name in ("memory", "correctness", "security")
    }
    unknown_regression_guard_reasons = sorted(
        reason
        for reason in normalized_failure_or_gap_reason_set
        if reason not in known_regression_guard_reasons
    )
    require_condition(
        "claim_integrity.phase1_matrix_regression_guard_reasons_known",
        path=perf_phase1_matrix_validation_path,
        ok=not unknown_regression_guard_reasons,
        ok_msg="phase-1 matrix regression guard reasons use known tokens",
        fail_msg=(
            "phase-1 matrix regression guard reasons contain unknown tokens: "
            f"{unknown_regression_guard_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Use only <guard>_regression and <guard>_regression_unverified tokens "
            "for regression_guards.failure_or_gap_reasons."
        ),
    )
    if unknown_regression_guard_reasons and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation regression_guards.failure_or_gap_reasons "
            "contains unknown tokens"
        )

    invalid_regression_guard_statuses: list[str] = []
    regression_guard_reason_mismatches: list[str] = []
    expected_regression_guard_reason_set: set[str] = set()
    for guard_name in ("memory", "correctness", "security"):
        status_raw = (
            regression_guards_obj.get(guard_name)
            if isinstance(regression_guards_obj, dict)
            else None
        )
        status = str(status_raw or "").strip().lower()
        if status not in {"pass", "fail", "missing"}:
            invalid_regression_guard_statuses.append(f"{guard_name}={status_raw!r}")
            continue
        fail_reason_token = f"{guard_name}_regression"
        missing_reason_token = f"{guard_name}_regression_unverified"
        has_fail_reason = fail_reason_token in normalized_failure_or_gap_reason_set
        has_missing_reason = (
            missing_reason_token in normalized_failure_or_gap_reason_set
        )
        if status == "fail":
            expected_regression_guard_reason_set.add(fail_reason_token)
        elif status == "missing":
            expected_regression_guard_reason_set.add(missing_reason_token)
        if status == "pass" and (has_fail_reason or has_missing_reason):
            regression_guard_reason_mismatches.append(
                f"{guard_name}=pass with {fail_reason_token}/{missing_reason_token}"
            )
        elif status == "fail" and (not has_fail_reason or has_missing_reason):
            regression_guard_reason_mismatches.append(
                f"{guard_name}=fail requires {fail_reason_token} only"
            )
        elif status == "missing" and (not has_missing_reason or has_fail_reason):
            regression_guard_reason_mismatches.append(
                f"{guard_name}=missing requires {missing_reason_token} only"
            )

    require_condition(
        "claim_integrity.phase1_matrix_regression_guard_status_valid",
        path=perf_phase1_matrix_validation_path,
        ok=not invalid_regression_guard_statuses,
        ok_msg="phase-1 matrix regression guard statuses are pass/fail/missing",
        fail_msg=(
            "phase-1 matrix regression guard status invalid entries: "
            f"{invalid_regression_guard_statuses}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set regression_guards.memory/correctness/security statuses to one of "
            "'pass', 'fail', or 'missing' in scripts/perf/orchestrate.sh."
        ),
    )
    if invalid_regression_guard_statuses and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation regression_guards statuses must be "
            "pass/fail/missing"
        )

    regression_guard_reason_set_exact = (
        regression_guard_reason_format_ok
        and not unknown_regression_guard_reasons
        and not invalid_regression_guard_statuses
        and normalized_failure_or_gap_reason_set == expected_regression_guard_reason_set
    )
    require_condition(
        "claim_integrity.phase1_matrix_regression_guard_reason_set_exact",
        path=perf_phase1_matrix_validation_path,
        ok=regression_guard_reason_set_exact,
        ok_msg=(
            "phase-1 matrix regression guard reasons exactly match derived "
            "status-required token set"
        ),
        fail_msg=(
            "phase-1 matrix regression guard reasons must exactly match status-derived "
            f"set: expected={sorted(expected_regression_guard_reason_set)}, "
            f"observed={sorted(normalized_failure_or_gap_reason_set)}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit exactly one token per non-pass guard status: "
            "<guard>_regression for fail and <guard>_regression_unverified for missing."
        ),
    )
    if not regression_guard_reason_set_exact and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation regression_guards.failure_or_gap_reasons "
            "must exactly match derived non-pass guard statuses"
        )

    require_condition(
        "claim_integrity.phase1_matrix_regression_guard_reason_alignment",
        path=perf_phase1_matrix_validation_path,
        ok=not regression_guard_reason_mismatches,
        ok_msg="phase-1 matrix regression guard statuses align with reason tokens",
        fail_msg=(
            "phase-1 matrix regression guard status/reason mismatches: "
            f"{regression_guard_reason_mismatches}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Align regression_guards.failure_or_gap_reasons with guard statuses: "
            "<guard>_regression for fail, <guard>_regression_unverified for missing, "
            "and neither token for pass."
        ),
    )
    if regression_guard_reason_mismatches and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation regression_guards status/reason alignment failed"
        )

    consumption_contract = phase1_matrix_validation.get("consumption_contract")
    consumption_contract_obj = (
        consumption_contract if isinstance(consumption_contract, dict) else None
    )
    require_condition(
        "claim_integrity.phase1_matrix_consumption_contract_object",
        path=perf_phase1_matrix_validation_path,
        ok=consumption_contract_obj is not None,
        ok_msg="phase-1 matrix validation consumption_contract object present",
        fail_msg="phase-1 matrix validation missing consumption_contract object",
        strict=claim_integrity_required,
        remediation=(
            "Emit consumption_contract in scripts/perf/orchestrate.sh with "
            "downstream_beads/downstream_consumers plus artifact_ready_for_phase5."
        ),
    )

    downstream_bead_ids: list[str] = []
    invalid_downstream_bead_entries: list[str] = []
    downstream_beads_raw = (
        consumption_contract_obj.get("downstream_beads")
        if isinstance(consumption_contract_obj, dict)
        else None
    )
    if isinstance(downstream_beads_raw, list):
        for index, raw_bead in enumerate(downstream_beads_raw):
            if not isinstance(raw_bead, str):
                invalid_downstream_bead_entries.append(
                    f"downstream_beads[{index}] must be a string"
                )
                continue
            bead_id = raw_bead.strip()
            if not bead_id:
                invalid_downstream_bead_entries.append(
                    f"downstream_beads[{index}] must be non-empty"
                )
                continue
            downstream_bead_ids.append(bead_id)
    elif consumption_contract_obj is not None:
        invalid_downstream_bead_entries.append("downstream_beads is not an array")

    required_phase5_downstream_beads = {"bd-3ar8v.6.1", "bd-3ar8v.6.2"}
    observed_downstream_bead_set = set(downstream_bead_ids)
    missing_phase5_downstream_beads = sorted(
        required_phase5_downstream_beads - observed_downstream_bead_set
    )
    downstream_beads_ok = (
        not invalid_downstream_bead_entries and not missing_phase5_downstream_beads
    )
    require_condition(
        "claim_integrity.phase1_matrix_downstream_beads_include_phase5",
        path=perf_phase1_matrix_validation_path,
        ok=downstream_beads_ok,
        ok_msg=(
            "phase-1 matrix validation downstream_beads include opportunity-matrix "
            "and parameter-sweep consumers"
        ),
        fail_msg=(
            "phase-1 matrix validation downstream_beads missing/invalid entries: "
            f"missing={missing_phase5_downstream_beads}, "
            f"invalid={invalid_downstream_bead_entries}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Include bd-3ar8v.6.1 and bd-3ar8v.6.2 in "
            "consumption_contract.downstream_beads in scripts/perf/orchestrate.sh."
        ),
    )
    if not downstream_beads_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation downstream_beads must include "
            "bd-3ar8v.6.1 and bd-3ar8v.6.2"
        )

    downstream_consumers_raw = (
        consumption_contract_obj.get("downstream_consumers")
        if isinstance(consumption_contract_obj, dict)
        else None
    )
    downstream_consumers_obj = (
        downstream_consumers_raw if isinstance(downstream_consumers_raw, dict) else None
    )
    expected_downstream_consumers = {
        "opportunity_matrix": {
            "bead_id": "bd-3ar8v.6.1",
            "selector": "weighted_bottleneck_attribution.global_ranking",
            "source_artifact": "phase1_matrix_validation",
        },
        "parameter_sweeps": {
            "bead_id": "bd-3ar8v.6.2",
            "selector": "weighted_bottleneck_attribution.per_scale",
            "source_artifact": "phase1_matrix_validation",
        },
    }
    downstream_consumer_mismatches: list[str] = []
    if downstream_consumers_obj is None:
        downstream_consumer_mismatches.append(
            "downstream_consumers must be an object"
        )
    else:
        for consumer_name, expected_fields in expected_downstream_consumers.items():
            consumer_entry = downstream_consumers_obj.get(consumer_name)
            if not isinstance(consumer_entry, dict):
                downstream_consumer_mismatches.append(
                    f"downstream_consumers.{consumer_name} must be an object"
                )
                continue
            for field_name, expected_value in expected_fields.items():
                observed_value = consumer_entry.get(field_name)
                if not isinstance(observed_value, str) or observed_value.strip() != expected_value:
                    downstream_consumer_mismatches.append(
                        f"downstream_consumers.{consumer_name}.{field_name} must be {expected_value!r}"
                    )
    downstream_consumers_ok = not downstream_consumer_mismatches
    require_condition(
        "claim_integrity.phase1_matrix_downstream_consumers_contract",
        path=perf_phase1_matrix_validation_path,
        ok=downstream_consumers_ok,
        ok_msg=(
            "phase-1 matrix downstream_consumers explicitly map weighted attribution "
            "into phase-5 opportunity/sweep consumers"
        ),
        fail_msg=(
            "phase-1 matrix downstream_consumers contract mismatch: "
            f"{downstream_consumer_mismatches}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set consumption_contract.downstream_consumers.{opportunity_matrix,"
            "parameter_sweeps} with canonical bead_id/selector/source_artifact "
            "in scripts/perf/orchestrate.sh."
        ),
    )
    if not downstream_consumers_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation downstream_consumers contract mismatch for "
            "opportunity_matrix/parameter_sweeps"
        )

    weighted_bottleneck_raw = phase1_matrix_validation.get(
        "weighted_bottleneck_attribution"
    )
    weighted_bottleneck_obj = (
        weighted_bottleneck_raw if isinstance(weighted_bottleneck_raw, dict) else None
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_object",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_bottleneck_obj is not None,
        ok_msg="phase-1 matrix weighted_bottleneck_attribution object present",
        fail_msg=(
            "phase-1 matrix validation missing weighted_bottleneck_attribution object"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit weighted_bottleneck_attribution object with schema/status/"
            "lineage/per_scale/global_ranking in scripts/perf/orchestrate.sh."
        ),
    )
    if weighted_bottleneck_obj is None and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation missing weighted_bottleneck_attribution object"
        )

    weighted_bottleneck_schema = (
        str(weighted_bottleneck_obj.get("schema", "")).strip()
        if isinstance(weighted_bottleneck_obj, dict)
        else ""
    )
    weighted_bottleneck_schema_ok = (
        weighted_bottleneck_schema == "pi.perf.phase1_weighted_bottleneck_attribution.v1"
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_schema",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_bottleneck_schema_ok,
        ok_msg="phase-1 matrix weighted_bottleneck_attribution schema is canonical",
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution schema mismatch: "
            f"{weighted_bottleneck_schema or 'missing_or_non_string'}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set weighted_bottleneck_attribution.schema to "
            "pi.perf.phase1_weighted_bottleneck_attribution.v1 in "
            "scripts/perf/orchestrate.sh."
        ),
    )
    if not weighted_bottleneck_schema_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution schema mismatch"
        )

    weighted_bottleneck_status_raw = (
        weighted_bottleneck_obj.get("status")
        if isinstance(weighted_bottleneck_obj, dict)
        else None
    )
    weighted_bottleneck_status = (
        weighted_bottleneck_status_raw.strip().lower()
        if isinstance(weighted_bottleneck_status_raw, str)
        else ""
    )
    weighted_bottleneck_status_ok = weighted_bottleneck_status in {"computed", "missing"}
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_status_valid",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_bottleneck_status_ok,
        ok_msg=(
            "phase-1 matrix weighted_bottleneck_attribution.status is "
            "computed|missing"
        ),
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution.status must be "
            "computed|missing, got "
            f"{weighted_bottleneck_status_raw!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set weighted_bottleneck_attribution.status to computed or missing in "
            "scripts/perf/orchestrate.sh."
        ),
    )
    if not weighted_bottleneck_status_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution.status must "
            "be computed|missing"
        )

    weighted_bottleneck_per_scale_raw = (
        weighted_bottleneck_obj.get("per_scale")
        if isinstance(weighted_bottleneck_obj, dict)
        else None
    )
    weighted_bottleneck_global_ranking_raw = (
        weighted_bottleneck_obj.get("global_ranking")
        if isinstance(weighted_bottleneck_obj, dict)
        else None
    )
    weighted_bottleneck_per_scale = (
        weighted_bottleneck_per_scale_raw
        if isinstance(weighted_bottleneck_per_scale_raw, list)
        else None
    )
    weighted_bottleneck_global_ranking = (
        weighted_bottleneck_global_ranking_raw
        if isinstance(weighted_bottleneck_global_ranking_raw, list)
        else None
    )
    weighted_bottleneck_outputs_array_ok = (
        weighted_bottleneck_per_scale is not None
        and weighted_bottleneck_global_ranking is not None
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_outputs_array",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_bottleneck_outputs_array_ok,
        ok_msg=(
            "phase-1 matrix weighted_bottleneck_attribution per_scale/global_ranking "
            "arrays are present"
        ),
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution requires array fields: "
            f"per_scale={type(weighted_bottleneck_per_scale_raw).__name__}, "
            f"global_ranking={type(weighted_bottleneck_global_ranking_raw).__name__}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit weighted_bottleneck_attribution.per_scale and "
            "weighted_bottleneck_attribution.global_ranking as arrays in "
            "scripts/perf/orchestrate.sh."
        ),
    )
    if not weighted_bottleneck_outputs_array_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution output arrays "
            "missing/invalid"
        )

    weighted_bottleneck_lineage_raw = (
        weighted_bottleneck_obj.get("lineage")
        if isinstance(weighted_bottleneck_obj, dict)
        else None
    )
    weighted_bottleneck_lineage = (
        weighted_bottleneck_lineage_raw
        if isinstance(weighted_bottleneck_lineage_raw, dict)
        else None
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_lineage_object",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_bottleneck_lineage is not None,
        ok_msg="phase-1 matrix weighted_bottleneck_attribution.lineage object present",
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution.lineage must be an object"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit weighted_bottleneck_attribution.lineage with "
            "source_cell_count and valid_cell_count in scripts/perf/orchestrate.sh."
        ),
    )
    if weighted_bottleneck_lineage is None and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution.lineage missing"
        )

    def parse_non_negative_weighted_count(raw: object) -> int | None:
        if isinstance(raw, bool):
            return None
        if isinstance(raw, int):
            return raw if raw >= 0 else None
        if isinstance(raw, str):
            candidate = raw.strip()
            if candidate.isdigit():
                return int(candidate)
        return None

    weighted_source_cell_count = parse_non_negative_weighted_count(
        weighted_bottleneck_lineage.get("source_cell_count")
        if isinstance(weighted_bottleneck_lineage, dict)
        else None
    )
    weighted_valid_cell_count = parse_non_negative_weighted_count(
        weighted_bottleneck_lineage.get("valid_cell_count")
        if isinstance(weighted_bottleneck_lineage, dict)
        else None
    )
    weighted_lineage_counts_present = (
        weighted_source_cell_count is not None
        and weighted_valid_cell_count is not None
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_lineage_counts_present",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_lineage_counts_present,
        ok_msg=(
            "phase-1 matrix weighted_bottleneck_attribution.lineage count fields "
            "are present and non-negative"
        ),
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution.lineage requires "
            "non-negative integer source_cell_count/valid_cell_count: "
            f"source={weighted_source_cell_count!r}, "
            f"valid={weighted_valid_cell_count!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit non-negative integer lineage.source_cell_count and "
            "lineage.valid_cell_count in scripts/perf/orchestrate.sh."
        ),
    )
    if not weighted_lineage_counts_present and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution lineage counts "
            "missing/invalid"
        )

    weighted_lineage_bounds_ok = (
        weighted_lineage_counts_present
        and weighted_valid_cell_count <= weighted_source_cell_count
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_lineage_bounds",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_lineage_bounds_ok,
        ok_msg=(
            "phase-1 matrix weighted_bottleneck_attribution lineage preserves "
            "valid_cell_count <= source_cell_count"
        ),
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution lineage bounds invalid: "
            f"source_cell_count={weighted_source_cell_count!r}, "
            f"valid_cell_count={weighted_valid_cell_count!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Keep weighted_bottleneck_attribution.lineage.valid_cell_count <= "
            "lineage.source_cell_count in scripts/perf/orchestrate.sh."
        ),
    )
    if not weighted_lineage_bounds_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution lineage "
            "bounds invalid (valid_cell_count > source_cell_count)"
        )

    matrix_cells_raw_for_weighted = phase1_matrix_validation.get("matrix_cells")
    matrix_cells_for_weighted = (
        matrix_cells_raw_for_weighted
        if isinstance(matrix_cells_raw_for_weighted, list)
        else None
    )
    weighted_lineage_source_matches_matrix = (
        not isinstance(matrix_cells_for_weighted, list)
        or not weighted_lineage_counts_present
        or weighted_source_cell_count == len(matrix_cells_for_weighted)
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_lineage_source_matches_matrix_cells",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_lineage_source_matches_matrix,
        ok_msg=(
            "phase-1 matrix weighted_bottleneck_attribution lineage source count "
            "matches matrix_cells length"
        ),
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution lineage source count "
            "must match matrix_cells length: "
            f"lineage.source_cell_count={weighted_source_cell_count!r}, "
            f"matrix_cells={len(matrix_cells_for_weighted) if isinstance(matrix_cells_for_weighted, list) else 'n/a'}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set weighted_bottleneck_attribution.lineage.source_cell_count to "
            "phase1_matrix_validation.matrix_cells length in scripts/perf/orchestrate.sh."
        ),
    )
    if not weighted_lineage_source_matches_matrix and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution lineage "
            "source_cell_count mismatch with matrix_cells length"
        )

    weighted_per_scale_len = (
        len(weighted_bottleneck_per_scale)
        if isinstance(weighted_bottleneck_per_scale, list)
        else 0
    )
    weighted_global_ranking_len = (
        len(weighted_bottleneck_global_ranking)
        if isinstance(weighted_bottleneck_global_ranking, list)
        else 0
    )
    weighted_status_coherence_ok = (
        weighted_lineage_counts_present
        and weighted_bottleneck_status_ok
        and (
            (
                weighted_bottleneck_status == "missing"
                and weighted_valid_cell_count == 0
                and weighted_per_scale_len == 0
                and weighted_global_ranking_len == 0
            )
            or (
                weighted_bottleneck_status == "computed"
                and weighted_valid_cell_count > 0
                and weighted_per_scale_len > 0
                and weighted_global_ranking_len > 0
            )
        )
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_status_coherence",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_status_coherence_ok,
        ok_msg=(
            "phase-1 matrix weighted_bottleneck_attribution status coheres with "
            "lineage/output cardinalities"
        ),
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution status coherence failed: "
            f"status={weighted_bottleneck_status_raw!r}, "
            f"valid_cell_count={weighted_valid_cell_count!r}, "
            f"per_scale_len={weighted_per_scale_len}, "
            f"global_ranking_len={weighted_global_ranking_len}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "When status=missing set lineage.valid_cell_count=0 with empty "
            "per_scale/global_ranking; when status=computed require "
            "lineage.valid_cell_count>0 and non-empty outputs in "
            "scripts/perf/orchestrate.sh."
        ),
    )
    if not weighted_status_coherence_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution status/output "
            "coherence failed"
        )

    expected_weighted_stage_keys = {"open_ms", "append_ms", "save_ms", "index_ms"}
    weighted_stage_rows_invalid: list[str] = []
    weighted_observed_stage_keys: set[str] = set()
    if weighted_bottleneck_status == "computed" and isinstance(
        weighted_bottleneck_global_ranking, list
    ):
        for index, row in enumerate(weighted_bottleneck_global_ranking):
            if not isinstance(row, dict):
                weighted_stage_rows_invalid.append(
                    f"global_ranking[{index}] must be an object"
                )
                continue
            stage_raw = row.get("stage")
            if not isinstance(stage_raw, str) or not stage_raw.strip():
                weighted_stage_rows_invalid.append(
                    f"global_ranking[{index}].stage must be a non-empty string"
                )
                continue
            weighted_observed_stage_keys.add(stage_raw.strip().lower())
    weighted_stage_coverage_ok = (
        weighted_bottleneck_status != "computed"
        or (
            not weighted_stage_rows_invalid
            and weighted_observed_stage_keys == expected_weighted_stage_keys
        )
    )
    require_condition(
        "claim_integrity.phase1_matrix_weighted_bottleneck_stage_coverage",
        path=perf_phase1_matrix_validation_path,
        ok=weighted_stage_coverage_ok,
        ok_msg=(
            "phase-1 matrix weighted_bottleneck_attribution.global_ranking has "
            "canonical stage coverage"
        ),
        fail_msg=(
            "phase-1 matrix weighted_bottleneck_attribution.global_ranking stage "
            "coverage mismatch: "
            f"observed={sorted(weighted_observed_stage_keys)}, "
            f"expected={sorted(expected_weighted_stage_keys)}, "
            f"invalid={weighted_stage_rows_invalid}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "When weighted_bottleneck_attribution.status=computed, populate "
            "global_ranking rows with canonical stages "
            "open_ms/append_ms/save_ms/index_ms in scripts/perf/orchestrate.sh."
        ),
    )
    if not weighted_stage_coverage_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation weighted_bottleneck_attribution stage coverage "
            "mismatch for global_ranking"
        )

    matrix_cells = phase1_matrix_validation.get("matrix_cells")
    matrix_cells_list = matrix_cells if isinstance(matrix_cells, list) else []
    stage_summary = phase1_matrix_validation.get("stage_summary")
    stage_summary_obj = stage_summary if isinstance(stage_summary, dict) else None
    require_condition(
        "claim_integrity.phase1_matrix_stage_summary_object",
        path=perf_phase1_matrix_validation_path,
        ok=stage_summary_obj is not None,
        ok_msg="phase-1 matrix validation stage_summary object present",
        fail_msg="phase-1 matrix validation missing stage_summary object",
        strict=claim_integrity_required,
        remediation=(
            "Update scripts/perf/orchestrate.sh to emit stage_summary with "
            "required_stage_keys, required_cell_count, "
            "cells_with_complete_stage_breakdown, and "
            "cells_missing_stage_breakdown."
        ),
    )

    expected_stage_keys = ["open_ms", "append_ms", "save_ms", "index_ms"]
    required_stage_keys_raw = (
        stage_summary_obj.get("required_stage_keys")
        if isinstance(stage_summary_obj, dict)
        else None
    )
    normalized_required_stage_keys: list[str] = []
    invalid_required_stage_keys: list[str] = []
    if isinstance(required_stage_keys_raw, list):
        for index, raw_stage_key in enumerate(required_stage_keys_raw):
            if not isinstance(raw_stage_key, str):
                invalid_required_stage_keys.append(
                    f"required_stage_keys[{index}] non-string {raw_stage_key!r}"
                )
                continue
            normalized_stage_key = raw_stage_key.strip().lower()
            if not normalized_stage_key:
                invalid_required_stage_keys.append(
                    f"required_stage_keys[{index}] empty stage key"
                )
                continue
            normalized_required_stage_keys.append(normalized_stage_key)
    elif stage_summary_obj is not None:
        invalid_required_stage_keys.append(
            "required_stage_keys is not an array"
        )

    required_stage_keys_exact = (
        not invalid_required_stage_keys
        and normalized_required_stage_keys == expected_stage_keys
    )
    require_condition(
        "claim_integrity.phase1_matrix_required_stage_keys_exact",
        path=perf_phase1_matrix_validation_path,
        ok=required_stage_keys_exact,
        ok_msg="phase-1 matrix stage_summary required_stage_keys include index_ms",
        fail_msg=(
            "phase-1 matrix stage_summary required_stage_keys mismatch: expected "
            f"{expected_stage_keys}, got {normalized_required_stage_keys}, "
            f"invalid={invalid_required_stage_keys}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set stage_summary.required_stage_keys exactly to "
            "['open_ms','append_ms','save_ms','index_ms'] in "
            "scripts/perf/orchestrate.sh."
        ),
    )
    if not required_stage_keys_exact and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation stage_summary.required_stage_keys must include "
            "open_ms/append_ms/save_ms/index_ms in canonical order"
        )

    def parse_non_negative_count(raw: object) -> int | None:
        if isinstance(raw, bool):
            return None
        if isinstance(raw, int):
            return raw if raw >= 0 else None
        if isinstance(raw, str):
            candidate = raw.strip()
            if candidate.isdigit():
                return int(candidate)
        return None

    complete_stage_cells = parse_non_negative_count(
        stage_summary_obj.get("cells_with_complete_stage_breakdown")
        if isinstance(stage_summary_obj, dict)
        else None
    )
    missing_stage_cells = parse_non_negative_count(
        stage_summary_obj.get("cells_missing_stage_breakdown")
        if isinstance(stage_summary_obj, dict)
        else None
    )
    required_stage_cells = parse_non_negative_count(
        stage_summary_obj.get("required_cell_count")
        if isinstance(stage_summary_obj, dict)
        else None
    )
    stage_summary_counts_coherent = (
        complete_stage_cells is not None
        and missing_stage_cells is not None
        and required_stage_cells is not None
        and complete_stage_cells + missing_stage_cells == required_stage_cells
    )
    require_condition(
        "claim_integrity.phase1_matrix_stage_summary_counts_coherent",
        path=perf_phase1_matrix_validation_path,
        ok=stage_summary_counts_coherent,
        ok_msg="phase-1 matrix stage_summary complete/missing counts are coherent",
        fail_msg=(
            "phase-1 matrix stage_summary count mismatch: "
            f"required_cell_count={required_stage_cells!r}, "
            f"cells_with_complete_stage_breakdown={complete_stage_cells!r}, "
            f"cells_missing_stage_breakdown={missing_stage_cells!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Ensure stage_summary.required_cell_count equals "
            "cells_with_complete_stage_breakdown + cells_missing_stage_breakdown "
            "in scripts/perf/orchestrate.sh."
        ),
    )
    if not stage_summary_counts_coherent and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation stage_summary counts must satisfy "
            "required_cell_count == complete + missing"
        )

    missing_stage_metric_reason_cells = 0
    invalid_missing_stage_metric_reason_cells: list[str] = []
    for index, cell in enumerate(matrix_cells_list):
        if not isinstance(cell, dict):
            continue
        failure_reasons = cell.get("failure_reasons")
        if failure_reasons is None:
            continue
        if not isinstance(failure_reasons, list):
            invalid_missing_stage_metric_reason_cells.append(
                f"cell[{index}] failure_reasons is not an array"
            )
            continue
        has_missing_stage_metrics_reason = False
        for raw_reason in failure_reasons:
            if not isinstance(raw_reason, str):
                continue
            normalized_reason = raw_reason.strip().lower()
            if not normalized_reason.startswith("missing_stage_metrics:"):
                continue
            missing_suffix = normalized_reason.split(":", 1)[1].strip()
            if missing_suffix:
                has_missing_stage_metrics_reason = True
            else:
                invalid_missing_stage_metric_reason_cells.append(
                    f"cell[{index}] has empty missing_stage_metrics reason token"
                )
        if has_missing_stage_metrics_reason:
            missing_stage_metric_reason_cells += 1
    missing_stage_metrics_visibility_ok = (
        not invalid_missing_stage_metric_reason_cells
        and missing_stage_cells is not None
        and missing_stage_metric_reason_cells == missing_stage_cells
    )
    require_condition(
        "claim_integrity.phase1_matrix_missing_stage_metrics_visibility",
        path=perf_phase1_matrix_validation_path,
        ok=missing_stage_metrics_visibility_ok,
        ok_msg=(
            "phase-1 matrix stage_summary missing-stage count matches matrix cell "
            "missing_stage_metrics reasons"
        ),
        fail_msg=(
            "phase-1 matrix missing_stage_metrics visibility mismatch: "
            f"cells_missing_stage_breakdown={missing_stage_cells!r}, "
            f"matrix_cells_with_missing_stage_metrics_reason="
            f"{missing_stage_metric_reason_cells}, "
            f"invalid={invalid_missing_stage_metric_reason_cells}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit a non-empty missing_stage_metrics:<csv> reason token on each matrix "
            "cell missing any required stage metric and keep "
            "stage_summary.cells_missing_stage_breakdown aligned."
        ),
    )
    if not missing_stage_metrics_visibility_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation missing_stage_metrics visibility mismatch "
            "between stage_summary and matrix cell failure_reasons"
        )

    invalid_matrix_cell_primary_e2e: list[str] = []
    for index, cell in enumerate(matrix_cells_list):
        if not isinstance(cell, dict):
            invalid_matrix_cell_primary_e2e.append(f"cell[{index}] is not an object")
            continue
        cell_status = normalize_claim_outcome(cell.get("status"))
        if cell_status not in {"pass", "fail"}:
            invalid_matrix_cell_primary_e2e.append(
                f"cell[{index}] status must be pass/fail, got {cell.get('status')!r}"
            )
            continue
        primary_e2e = cell.get("primary_e2e")
        primary_e2e_obj = primary_e2e if isinstance(primary_e2e, dict) else None
        invalid_fields: list[str] = []
        for field in ("wall_clock_ms", "rust_vs_node_ratio", "rust_vs_bun_ratio"):
            raw_metric = (
                primary_e2e_obj.get(field) if isinstance(primary_e2e_obj, dict) else None
            )
            metric_value = parse_positive_metric_value(
                raw_metric
            )
            if metric_value is None:
                if cell_status == "pass":
                    invalid_fields.append(field)
                elif not metric_value_missing(raw_metric):
                    invalid_fields.append(f"{field}(non-positive)")
        if invalid_fields:
            invalid_matrix_cell_primary_e2e.append(
                f"cell[{index}] primary_e2e missing/invalid {invalid_fields}"
            )
    invalid_matrix_cell_primary_e2e_excerpt = (
        invalid_matrix_cell_primary_e2e[:8]
        + (["..."] if len(invalid_matrix_cell_primary_e2e) > 8 else [])
    )
    require_condition(
        "claim_integrity.phase1_matrix_cells_primary_e2e_metrics_present",
        path=perf_phase1_matrix_validation_path,
        ok=not invalid_matrix_cell_primary_e2e,
        ok_msg=(
            "phase-1 matrix validation matrix_cells enforce pass/fail-aware "
            "primary_e2e metric semantics"
        ),
        fail_msg=(
            "phase-1 matrix validation matrix_cells include missing/invalid "
            f"primary_e2e metrics: {invalid_matrix_cell_primary_e2e_excerpt}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set matrix cell status to pass/fail. Pass cells must include positive "
            "wall_clock_ms, rust_vs_node_ratio, and rust_vs_bun_ratio values; "
            "fail cells may omit metrics (null/blank) but any provided value must "
            "still be a positive finite number in scripts/perf/orchestrate.sh."
        ),
    )
    if invalid_matrix_cell_primary_e2e and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "phase-1 matrix validation matrix_cells include missing/invalid "
            "primary_e2e metrics"
        )

    if required_realistic_session_shapes and not matrix_cells_list:
        missing_realistic_session_shape_reasons.append(
            "phase-1 matrix validation missing matrix_cells data for realistic tier coverage"
        )
    for index, cell in enumerate(matrix_cells_list):
        if not isinstance(cell, dict):
            continue
        partition = str(cell.get("workload_partition", "")).strip().lower()
        if partition != "realistic":
            continue
        session_messages = parse_session_messages_value(cell.get("session_messages"))
        if session_messages is None:
            missing_realistic_session_shape_reasons.append(
                f"phase1 matrix cell[{index}] missing/invalid session_messages for realistic partition"
            )
            continue
        mapped_shape = required_realistic_shape_message_counts.get(session_messages)
        if mapped_shape is None and required_realistic_shape_message_counts:
            missing_realistic_session_shape_reasons.append(
                f"phase1 matrix cell[{index}] session_messages "
                f"{session_messages} not mapped to required realistic tiers"
            )
            continue
        if mapped_shape:
            phase1_realistic_session_shapes_observed.add(mapped_shape)

if isinstance(parameter_sweeps, dict) and perf_parameter_sweeps_path is not None:
    require_keys(
        "claim_integrity.parameter_sweeps_json",
        parameter_sweeps,
        perf_parameter_sweeps_path,
        [
            "schema",
            "generated_at",
            "run_id",
            "correlation_id",
            "source_identity",
            "sweep_plan",
            "selected_defaults",
            "readiness",
        ],
        strict=claim_integrity_required,
    )
    require_condition(
        "claim_integrity.parameter_sweeps_schema",
        path=perf_parameter_sweeps_path,
        ok=parameter_sweeps.get("schema") == "pi.perf.parameter_sweeps.v1",
        ok_msg="parameter_sweeps schema matches",
        fail_msg=(
            "parameter_sweeps schema mismatch: expected 'pi.perf.parameter_sweeps.v1'"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set parameter_sweeps.schema to pi.perf.parameter_sweeps.v1 in "
            "scripts/perf/orchestrate.sh."
        ),
    )
    if (
        parameter_sweeps.get("schema") != "pi.perf.parameter_sweeps.v1"
        and claim_integrity_gate_active
    ):
        evidence_missing_or_stale_reasons.append(
            "parameter_sweeps schema mismatch (expected pi.perf.parameter_sweeps.v1)"
        )

    validate_generated_at_freshness(
        payload=parameter_sweeps,
        payload_path=perf_parameter_sweeps_path,
        check_id="claim_integrity.parameter_sweeps_generated_at_fresh",
        label="parameter_sweeps",
    )

    sweeps_correlation_id = str(parameter_sweeps.get("correlation_id", "")).strip()
    sweeps_correlation_ok = (
        bool(sweeps_correlation_id)
        and bool(expected_claim_correlation_id)
        and sweeps_correlation_id == expected_claim_correlation_id
    )
    require_condition(
        "claim_integrity.parameter_sweeps_correlation_matches_run",
        path=perf_parameter_sweeps_path,
        ok=sweeps_correlation_ok,
        ok_msg="parameter_sweeps correlation_id matches run summary/environment",
        fail_msg=(
            "parameter_sweeps correlation_id mismatch: "
            f"expected={expected_claim_correlation_id!r}, observed={sweeps_correlation_id!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set parameter_sweeps.correlation_id from orchestrate run correlation "
            "and keep it aligned with run_all summary/environment."
        ),
    )
    if not sweeps_correlation_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "parameter_sweeps correlation_id mismatch with run summary/environment"
        )

    source_identity_raw = parameter_sweeps.get("source_identity")
    source_identity_obj = (
        source_identity_raw if isinstance(source_identity_raw, dict) else None
    )
    require_condition(
        "claim_integrity.parameter_sweeps_source_identity_object",
        path=perf_parameter_sweeps_path,
        ok=source_identity_obj is not None,
        ok_msg="parameter_sweeps source_identity object present",
        fail_msg="parameter_sweeps source_identity must be an object",
        strict=claim_integrity_required,
        remediation=(
            "Emit parameter_sweeps.source_identity in scripts/perf/orchestrate.sh."
        ),
    )

    source_artifact = (
        str(source_identity_obj.get("source_artifact", "")).strip()
        if isinstance(source_identity_obj, dict)
        else ""
    )
    source_artifact_ok = source_artifact == "phase1_matrix_validation"
    require_condition(
        "claim_integrity.parameter_sweeps_source_identity",
        path=perf_parameter_sweeps_path,
        ok=source_artifact_ok,
        ok_msg="parameter_sweeps source_identity references phase1_matrix_validation",
        fail_msg=(
            "parameter_sweeps source_identity.source_artifact must be "
            f"'phase1_matrix_validation', got {source_artifact!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set parameter_sweeps.source_identity.source_artifact to "
            "'phase1_matrix_validation' in scripts/perf/orchestrate.sh."
        ),
    )
    if not source_artifact_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "parameter_sweeps source_identity.source_artifact mismatch"
        )

    source_artifact_path = (
        str(source_identity_obj.get("source_artifact_path", "")).strip()
        if isinstance(source_identity_obj, dict)
        else ""
    )
    source_artifact_path_ok = bool(source_artifact_path)
    if source_artifact_path_ok:
        normalized_source_path = source_artifact_path.replace("\\", "/")
        source_artifact_path_ok = normalized_source_path.endswith(
            "/phase1_matrix_validation.json"
        ) or normalized_source_path.endswith("phase1_matrix_validation.json")
        if source_artifact_path_ok and perf_phase1_matrix_validation_path is not None:
            expected_phase1_path = str(perf_phase1_matrix_validation_path).replace("\\", "/")
            source_artifact_path_ok = normalized_source_path.endswith(expected_phase1_path)
    require_condition(
        "claim_integrity.parameter_sweeps_source_artifact_path_matches_phase1",
        path=perf_parameter_sweeps_path,
        ok=source_artifact_path_ok,
        ok_msg="parameter_sweeps source artifact path links to phase1 matrix artifact",
        fail_msg=(
            "parameter_sweeps source_identity.source_artifact_path must resolve to "
            f"phase1_matrix_validation.json (observed={source_artifact_path!r})"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set parameter_sweeps.source_identity.source_artifact_path to the emitted "
            "phase1_matrix_validation.json path in scripts/perf/orchestrate.sh."
        ),
    )

    readiness_raw = parameter_sweeps.get("readiness")
    readiness_obj = readiness_raw if isinstance(readiness_raw, dict) else None
    require_condition(
        "claim_integrity.parameter_sweeps_readiness_object",
        path=perf_parameter_sweeps_path,
        ok=readiness_obj is not None,
        ok_msg="parameter_sweeps readiness object present",
        fail_msg="parameter_sweeps readiness must be an object",
        strict=claim_integrity_required,
        remediation=(
            "Emit parameter_sweeps.readiness in scripts/perf/orchestrate.sh."
        ),
    )

    readiness_status = (
        str(readiness_obj.get("status", "")).strip().lower()
        if isinstance(readiness_obj, dict)
        else ""
    )
    readiness_flag = (
        readiness_obj.get("ready_for_phase5")
        if isinstance(readiness_obj, dict)
        else None
    )
    blocking_reasons = (
        readiness_obj.get("blocking_reasons")
        if isinstance(readiness_obj, dict)
        else None
    )
    readiness_status_coherence_ok = (
        readiness_status in {"ready", "blocked"}
        and isinstance(readiness_flag, bool)
        and isinstance(blocking_reasons, list)
        and (
            (
                readiness_status == "ready"
                and readiness_flag is True
                and len(blocking_reasons) == 0
            )
            or (
                readiness_status == "blocked"
                and readiness_flag is False
                and len(blocking_reasons) > 0
            )
        )
    )
    require_condition(
        "claim_integrity.parameter_sweeps_readiness_status_coherence",
        path=perf_parameter_sweeps_path,
        ok=readiness_status_coherence_ok,
        ok_msg="parameter_sweeps readiness status/flags are coherent",
        fail_msg=(
            "parameter_sweeps readiness status coherence failed: "
            f"status={readiness_status!r}, ready_for_phase5={readiness_flag!r}, "
            f"blocking_reasons={blocking_reasons!r}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Set parameter_sweeps.readiness so ready => ready_for_phase5=true + empty "
            "blocking_reasons; blocked => ready_for_phase5=false + non-empty "
            "blocking_reasons."
        ),
    )

    selected_defaults_raw = parameter_sweeps.get("selected_defaults")
    selected_defaults_obj = (
        selected_defaults_raw if isinstance(selected_defaults_raw, dict) else None
    )
    required_default_keys = ["flush_cadence_ms", "queue_max_items", "compaction_quota_mb"]
    selected_defaults_ok = isinstance(selected_defaults_obj, dict)
    invalid_selected_defaults: list[str] = []
    if isinstance(selected_defaults_obj, dict):
        for key in required_default_keys:
            parsed = parse_session_messages_value(selected_defaults_obj.get(key))
            if parsed is None:
                invalid_selected_defaults.append(key)
    else:
        invalid_selected_defaults.extend(required_default_keys)

    selected_defaults_ok = selected_defaults_ok and not invalid_selected_defaults
    require_condition(
        "claim_integrity.parameter_sweeps_selected_defaults_fields",
        path=perf_parameter_sweeps_path,
        ok=selected_defaults_ok,
        ok_msg=(
            "parameter_sweeps selected_defaults include positive integer values "
            "for flush/queue/compaction knobs"
        ),
        fail_msg=(
            "parameter_sweeps selected_defaults missing/invalid required fields: "
            f"{invalid_selected_defaults}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit positive integer selected_defaults for flush_cadence_ms, "
            "queue_max_items, and compaction_quota_mb in scripts/perf/orchestrate.sh."
        ),
    )

    sweep_plan_raw = parameter_sweeps.get("sweep_plan")
    sweep_plan_obj = sweep_plan_raw if isinstance(sweep_plan_raw, dict) else None
    dimensions_raw = (
        sweep_plan_obj.get("dimensions") if isinstance(sweep_plan_obj, dict) else None
    )
    dimensions_list = dimensions_raw if isinstance(dimensions_raw, list) else None
    required_dimension_names = {
        "flush_cadence_ms",
        "queue_max_items",
        "compaction_quota_mb",
    }
    observed_dimension_names: set[str] = set()
    invalid_dimension_reasons: list[str] = []
    if isinstance(dimensions_list, list):
        for index, dimension in enumerate(dimensions_list):
            if not isinstance(dimension, dict):
                invalid_dimension_reasons.append(
                    f"dimensions[{index}] must be an object"
                )
                continue
            name = str(dimension.get("name", "")).strip()
            if not name:
                invalid_dimension_reasons.append(
                    f"dimensions[{index}].name must be a non-empty string"
                )
                continue
            observed_dimension_names.add(name)
            candidates = dimension.get("candidate_values")
            if not isinstance(candidates, list) or not candidates:
                invalid_dimension_reasons.append(
                    f"dimensions[{index}].candidate_values must be a non-empty array"
                )
    else:
        invalid_dimension_reasons.append("sweep_plan.dimensions must be an array")

    dimensions_cover_core_knobs_ok = (
        not invalid_dimension_reasons
        and required_dimension_names.issubset(observed_dimension_names)
    )
    require_condition(
        "claim_integrity.parameter_sweeps_dimensions_cover_core_knobs",
        path=perf_parameter_sweeps_path,
        ok=dimensions_cover_core_knobs_ok,
        ok_msg=(
            "parameter_sweeps sweep_plan.dimensions cover core knobs "
            "flush/queue/compaction with candidate values"
        ),
        fail_msg=(
            "parameter_sweeps sweep_plan.dimensions invalid/missing core knobs: "
            f"observed={sorted(observed_dimension_names)}, "
            f"required={sorted(required_dimension_names)}, "
            f"invalid={invalid_dimension_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit sweep_plan.dimensions entries for flush_cadence_ms, "
            "queue_max_items, and compaction_quota_mb with non-empty "
            "candidate_values arrays in scripts/perf/orchestrate.sh."
        ),
    )
    if not dimensions_cover_core_knobs_ok and claim_integrity_gate_active:
        evidence_missing_or_stale_reasons.append(
            "parameter_sweeps sweep_plan.dimensions missing/invalid for core knobs"
        )

realistic_shape_coverage_path = perf_baseline_confidence_path
if isinstance(phase1_matrix_validation, dict) and perf_phase1_matrix_validation_path is not None:
    realistic_session_shape_coverage_source = "phase1_matrix_validation"
    realistic_shape_coverage_path = perf_phase1_matrix_validation_path
    observed_realistic_session_shapes = set(phase1_realistic_session_shapes_observed)

if required_realistic_session_shapes:
    missing_realistic_session_shapes = [
        shape
        for shape in required_realistic_session_shapes
        if shape not in observed_realistic_session_shapes
    ]
    if missing_realistic_session_shapes:
        missing_realistic_session_shape_reasons.append(
            "missing required realistic_session_shape coverage: "
            f"{missing_realistic_session_shapes}"
        )

if claim_integrity_gate_active:
    matrix_scenarios: list[str] = (
        sorted(required_scenarios)
        if required_scenarios
        else sorted(scenario_partition_coverage.keys())
    )
    matrix_partitions: list[str] = (
        list(required_partition_tags)
        if required_partition_tags
        else sorted(
            {
                partition
                for partitions in scenario_partition_coverage.values()
                for partition in partitions
            }
        )
    )
    if not matrix_partitions:
        matrix_partitions = ["matched-state", "realistic"]

    realistic_shape_summary = {
        "required": list(required_realistic_session_shapes),
        "present": sorted(observed_realistic_session_shapes),
        "missing": list(missing_realistic_session_shapes),
        "overall_status": "pass" if not missing_realistic_session_shapes else "fail",
        "source_record_stream": realistic_session_shape_coverage_source,
        "source_workload_path": str(realistic_shape_coverage_path)
        if realistic_shape_coverage_path is not None
        else None,
        "source": realistic_session_shape_coverage_source,
        "source_path": str(realistic_shape_coverage_path)
        if realistic_shape_coverage_path is not None
        else None,
    }

    cells: list[dict] = []
    passing_cells = 0
    failing_cells = 0

    for scenario_id in matrix_scenarios:
        present_partitions = scenario_partition_coverage.get(scenario_id, set())
        for partition in matrix_partitions:
            passed = partition in present_partitions
            if passed:
                passing_cells += 1
            else:
                failing_cells += 1
            cells.append(
                {
                    "scenario_id": scenario_id,
                    "workload_partition": partition,
                    "status": "pass" if passed else "fail",
                    "present_in_records": passed,
                    "source": "pi.perf.baseline_variance_confidence.v1.records",
                    "reason": (
                        "coverage present in baseline confidence records"
                        if passed
                        else (
                            "missing_matrix_source_record: missing required "
                            "scenario/partition record"
                        )
                    ),
                }
            )

    scenario_cell_status_payload = {
        "schema": "pi.claim_integrity.scenario_cell_status.v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "correlation_id": expected_claim_correlation_id,
        "artifact_dir": str(artifact_dir),
        "required_scenarios": matrix_scenarios,
        "required_partitions": matrix_partitions,
        "summary": {
            "total_cells": len(cells),
            "passing_cells": passing_cells,
            "failing_cells": failing_cells,
            "overall_status": "pass" if failing_cells == 0 else "fail",
            "realistic_session_shape_coverage": realistic_shape_summary,
        },
        "cells": cells,
    }

    scenario_cell_status_json_path = artifact_dir / "claim_integrity_scenario_cell_status.json"
    scenario_cell_status_json_path.write_text(
        json.dumps(scenario_cell_status_payload, indent=2) + "\n",
        encoding="utf-8",
    )

    scenario_cell_status_markdown_path = (
        artifact_dir / "claim_integrity_scenario_cell_status.md"
    )
    md_lines = [
        "# Claim-Integrity Scenario Cell Status",
        "",
        f"- Schema: `{scenario_cell_status_payload['schema']}`",
        f"- Correlation ID: `{expected_claim_correlation_id}`",
        f"- Total cells: `{len(cells)}`",
        f"- Passing cells: `{passing_cells}`",
        f"- Failing cells: `{failing_cells}`",
        (
            "- Realistic session-shape status: "
            f"`{realistic_shape_summary['overall_status']}`"
        ),
        (
            "- Required realistic session shapes: "
            f"`{', '.join(realistic_shape_summary['required'])}`"
        ),
        (
            "- Present realistic session shapes: "
            f"`{', '.join(realistic_shape_summary['present']) or 'none'}`"
        ),
        (
            "- Missing realistic session shapes: "
            f"`{', '.join(realistic_shape_summary['missing']) or 'none'}`"
        ),
        (
            "- Realistic session-shape source: "
            f"`{realistic_shape_summary['source']}`"
        ),
        (
            "- Realistic session-shape source path: "
            f"`{realistic_shape_summary['source_path'] or 'none'}`"
        ),
        "",
        "| Scenario | Partition | Status | Reason |",
        "| --- | --- | --- | --- |",
    ]
    for cell in cells:
        icon = "PASS" if cell["status"] == "pass" else "FAIL"
        md_lines.append(
            f"| `{cell['scenario_id']}` | `{cell['workload_partition']}` | {icon} | {cell['reason']} |"
        )
    scenario_cell_status_markdown_path.write_text(
        "\n".join(md_lines) + "\n",
        encoding="utf-8",
    )

    add_check(
        "claim_integrity.scenario_cell_status_json",
        scenario_cell_status_json_path,
        True,
        f"scenario-cell status JSON written: {scenario_cell_status_json_path}",
    )
    add_check(
        "claim_integrity.scenario_cell_status_markdown",
        scenario_cell_status_markdown_path,
        True,
        f"scenario-cell status markdown written: {scenario_cell_status_markdown_path}",
    )

if claim_integrity_gate_active:
    artifact_claim_metadata: dict[str, dict] = {}
    artifact_claim_metadata["baseline_confidence"] = build_artifact_claim_metadata(
        source="baseline_confidence",
        payload=baseline_confidence if isinstance(baseline_confidence, dict) else None,
        path=perf_baseline_confidence_path,
    )
    artifact_claim_metadata["extension_stratification"] = build_artifact_claim_metadata(
        source="extension_stratification",
        payload=extension_stratification if isinstance(extension_stratification, dict) else None,
        path=perf_extension_stratification_path,
    )
    artifact_claim_metadata["phase1_matrix_validation"] = build_artifact_claim_metadata(
        source="phase1_matrix_validation",
        payload=phase1_matrix_validation if isinstance(phase1_matrix_validation, dict) else None,
        path=perf_phase1_matrix_validation_path,
    )
    artifact_claim_metadata["release_readiness_summary"] = build_artifact_claim_metadata(
        source="release_readiness_summary",
        payload=release_readiness if isinstance(release_readiness, dict) else None,
        path=release_readiness_path,
    )
    artifact_claim_metadata["scenario_cell_status"] = build_artifact_claim_metadata(
        source="scenario_cell_status",
        payload=(
            scenario_cell_status_payload
            if isinstance(scenario_cell_status_payload, dict)
            else None
        ),
        path=scenario_cell_status_json_path,
    )

    claim_observations: list[dict] = []

    def add_claim_observation(
        *,
        claim_scope: str,
        claim_id: str,
        source: str,
        metric_scope: str,
        scenario_class: str,
        reported_outcome: object,
        confidence_label: object,
        scenario_id: str | None = None,
        workload_partition: str | None = None,
        details: dict | None = None,
    ) -> None:
        artifact_meta = artifact_claim_metadata.get(source, {})
        noncanonical_reasons: list[str] = []
        if not artifact_meta:
            noncanonical_reasons.append("missing_source_artifact_metadata")
        else:
            if not artifact_meta.get("fresh"):
                noncanonical_reasons.append("stale_or_missing_generated_at")
            if expected_claim_correlation_id and not artifact_meta.get("correlation_matches"):
                noncanonical_reasons.append("correlation_mismatch")
        claim_observations.append(
            {
                "claim_scope": claim_scope,
                "claim_id": claim_id,
                "source": source,
                "metric_scope": metric_scope,
                "scenario_class": scenario_class,
                "scenario_id": scenario_id,
                "workload_partition": workload_partition,
                "reported_outcome": normalize_claim_outcome(reported_outcome),
                "confidence": normalize_confidence_label(confidence_label),
                "confidence_rank": confidence_rank(confidence_label),
                "artifact": artifact_meta,
                "canonical": not noncanonical_reasons,
                "noncanonical_reasons": noncanonical_reasons,
                "details": details or {},
            }
        )

    if isinstance(release_readiness, dict):
        readiness_outcome = normalize_bool_outcome(release_readiness.get("overall_ready"))
        if readiness_outcome == "unknown":
            readiness_outcome = normalize_claim_outcome(release_readiness.get("status"))
        add_claim_observation(
            claim_scope="release_global_claim",
            claim_id="release_readiness.overall_ready",
            source="release_readiness_summary",
            metric_scope="release_readiness",
            scenario_class="global",
            reported_outcome=readiness_outcome,
            confidence_label="medium" if readiness_outcome in {"pass", "fail"} else "low",
            details={
                "status": release_readiness.get("status"),
                "overall_ready": release_readiness.get("overall_ready"),
            },
        )

    if isinstance(extension_stratification, dict):
        layers_payload = extension_stratification.get("layers")
        layers_list = layers_payload if isinstance(layers_payload, list) else []
        claim_integrity_payload = extension_stratification.get("claim_integrity")
        cherry_pick_guard = (
            claim_integrity_payload.get("cherry_pick_guard", {})
            if isinstance(claim_integrity_payload, dict)
            else {}
        )
        invalidity_reasons = (
            cherry_pick_guard.get("invalidity_reasons", [])
            if isinstance(cherry_pick_guard, dict)
            else []
        )
        layer_confidences = [
            str(layer.get("confidence", "")).strip()
            for layer in layers_list
            if isinstance(layer, dict)
        ]
        add_claim_observation(
            claim_scope="release_global_claim",
            claim_id="extension_stratification.global_claim_valid",
            source="extension_stratification",
            metric_scope="global_claim_validity",
            scenario_class="global",
            reported_outcome=normalize_bool_outcome(
                cherry_pick_guard.get("global_claim_valid")
            ),
            confidence_label=highest_confidence_label(layer_confidences),
            details={
                "invalidity_reasons": sorted(
                    str(reason).strip()
                    for reason in invalidity_reasons
                    if str(reason).strip()
                ),
                "layer_coverage": (
                    cherry_pick_guard.get("layer_coverage", {})
                    if isinstance(cherry_pick_guard, dict)
                    else {}
                ),
            },
        )
        for layer in layers_list:
            if not isinstance(layer, dict):
                continue
            layer_id = str(layer.get("layer_id", "")).strip() or "unknown-layer"
            absolute_metrics = layer.get("absolute_metrics")
            relative_metrics = layer.get("relative_metrics")
            layer_pass = (
                isinstance(absolute_metrics, dict)
                and absolute_metrics.get("value") is not None
                and isinstance(relative_metrics, dict)
                and relative_metrics.get("rust_vs_node_ratio") is not None
                and relative_metrics.get("rust_vs_bun_ratio") is not None
            )
            add_claim_observation(
                claim_scope=f"extension_layer:{layer_id}",
                claim_id="extension_stratification.layer_coverage",
                source="extension_stratification",
                metric_scope="layer_coverage",
                scenario_class="layer",
                reported_outcome="pass" if layer_pass else "fail",
                confidence_label=layer.get("confidence"),
                details={
                    "layer_id": layer_id,
                    "evidence_state": layer.get("evidence_state"),
                    "relative_metrics": (
                        relative_metrics if isinstance(relative_metrics, dict) else {}
                    ),
                    "absolute_metrics": (
                        absolute_metrics if isinstance(absolute_metrics, dict) else {}
                    ),
                },
            )

    if isinstance(phase1_matrix_validation, dict):
        primary_outcomes = phase1_matrix_validation.get("primary_outcomes")
        primary_outcomes_obj = primary_outcomes if isinstance(primary_outcomes, dict) else {}
        primary_missing_reasons = (
            primary_outcomes_obj.get("missing_reasons", [])
            if isinstance(primary_outcomes_obj, dict)
            else []
        )
        primary_confidence = (
            "high"
            if not primary_missing_reasons
            else "low"
        )
        add_claim_observation(
            claim_scope="release_global_claim",
            claim_id="phase1_matrix_validation.primary_outcomes.status",
            source="phase1_matrix_validation",
            metric_scope="primary_outcomes",
            scenario_class="global",
            reported_outcome=primary_outcomes_obj.get("status"),
            confidence_label=primary_confidence,
            details={
                "missing_reasons": (
                    primary_missing_reasons if isinstance(primary_missing_reasons, list) else []
                ),
                "ordering_policy": primary_outcomes_obj.get("ordering_policy"),
            },
        )
        matrix_cells = phase1_matrix_validation.get("matrix_cells")
        matrix_cells_list = matrix_cells if isinstance(matrix_cells, list) else []
        for index, cell in enumerate(matrix_cells_list):
            if not isinstance(cell, dict):
                continue
            scenario_id = str(cell.get("scenario_id", "")).strip()
            workload_partition = str(cell.get("workload_partition", "")).strip()
            session_messages = parse_session_messages_value(cell.get("session_messages"))
            scope_suffix = (
                f"{session_messages}"
                if session_messages is not None
                else f"index-{index}"
            )
            primary_e2e = cell.get("primary_e2e")
            primary_e2e_obj = primary_e2e if isinstance(primary_e2e, dict) else {}
            cell_confidence = (
                "high"
                if (
                    parse_positive_metric_value(primary_e2e_obj.get("wall_clock_ms")) is not None
                    and parse_positive_metric_value(
                        primary_e2e_obj.get("rust_vs_node_ratio")
                    )
                    is not None
                    and parse_positive_metric_value(
                        primary_e2e_obj.get("rust_vs_bun_ratio")
                    )
                    is not None
                )
                else "low"
            )
            add_claim_observation(
                claim_scope=(
                    f"phase1_matrix_cell:{workload_partition or 'unknown'}:{scope_suffix}"
                ),
                claim_id="phase1_matrix_validation.matrix_cells.status",
                source="phase1_matrix_validation",
                metric_scope="matrix_cell_primary_e2e",
                scenario_class=workload_partition or "unknown",
                reported_outcome=cell.get("status"),
                confidence_label=cell_confidence,
                scenario_id=scenario_id or None,
                workload_partition=workload_partition or None,
                details={
                    "session_messages": session_messages,
                    "missing_reasons": (
                        cell.get("missing_reasons", [])
                        if isinstance(cell.get("missing_reasons"), list)
                        else []
                    ),
                },
            )

    if isinstance(scenario_cell_status_payload, dict):
        scenario_cells = scenario_cell_status_payload.get("cells")
        scenario_cells_list = scenario_cells if isinstance(scenario_cells, list) else []
        scenario_summary = scenario_cell_status_payload.get("summary")
        scenario_summary_obj = scenario_summary if isinstance(scenario_summary, dict) else {}

        baseline_confidence_counts = (
            baseline_confidence.get("summary", {}).get("confidence_counts", {})
            if isinstance(baseline_confidence, dict)
            else {}
        )
        confidence_from_counts = "unknown"
        if isinstance(baseline_confidence_counts, dict) and baseline_confidence_counts:
            sorted_confidence_counts = sorted(
                (
                    (normalize_confidence_label(label), int(count))
                    for label, count in baseline_confidence_counts.items()
                    if normalize_confidence_label(label) != "unknown"
                    and isinstance(count, (int, float))
                ),
                key=lambda item: (item[1], confidence_rank(item[0])),
                reverse=True,
            )
            if sorted_confidence_counts:
                confidence_from_counts = sorted_confidence_counts[0][0]

        add_claim_observation(
            claim_scope="release_global_claim",
            claim_id="scenario_cell_status.summary.overall_status",
            source="scenario_cell_status",
            metric_scope="scenario_partition_coverage",
            scenario_class="global",
            reported_outcome=scenario_summary_obj.get("overall_status"),
            confidence_label=confidence_from_counts,
            details={
                "total_cells": scenario_summary_obj.get("total_cells"),
                "passing_cells": scenario_summary_obj.get("passing_cells"),
                "failing_cells": scenario_summary_obj.get("failing_cells"),
            },
        )

        for cell in scenario_cells_list:
            if not isinstance(cell, dict):
                continue
            scenario_id = str(cell.get("scenario_id", "")).strip()
            workload_partition = str(cell.get("workload_partition", "")).strip()
            if not scenario_id or not workload_partition:
                continue
            confidence_labels = sorted(
                scenario_partition_confidence_labels.get(
                    (scenario_id, workload_partition),
                    set(),
                )
            )
            confidence_label = highest_confidence_label(confidence_labels)
            add_claim_observation(
                claim_scope=(
                    f"scenario_partition_coverage:{scenario_id}:{workload_partition}"
                ),
                claim_id="scenario_cell_status.cells.status",
                source="scenario_cell_status",
                metric_scope="scenario_partition_coverage",
                scenario_class=scenario_class_by_id.get(scenario_id, "unknown"),
                reported_outcome=cell.get("status"),
                confidence_label=confidence_label,
                scenario_id=scenario_id,
                workload_partition=workload_partition,
                details={
                    "reason": cell.get("reason"),
                    "present_in_records": cell.get("present_in_records"),
                },
            )

    baseline_matrix_scenarios: list[str] = (
        sorted(required_scenarios)
        if required_scenarios
        else sorted(scenario_partition_coverage.keys())
    )
    baseline_matrix_partitions: list[str] = (
        list(required_partition_tags)
        if required_partition_tags
        else sorted(
            {
                partition
                for partitions in scenario_partition_coverage.values()
                for partition in partitions
            }
        )
    )
    for scenario_id in baseline_matrix_scenarios:
        present_partitions = scenario_partition_coverage.get(scenario_id, set())
        for workload_partition in baseline_matrix_partitions:
            confidence_labels = sorted(
                scenario_partition_confidence_labels.get(
                    (scenario_id, workload_partition),
                    set(),
                )
            )
            add_claim_observation(
                claim_scope=(
                    f"scenario_partition_coverage:{scenario_id}:{workload_partition}"
                ),
                claim_id="baseline_confidence.records.coverage",
                source="baseline_confidence",
                metric_scope="scenario_partition_coverage",
                scenario_class=scenario_class_by_id.get(scenario_id, "unknown"),
                reported_outcome=(
                    "pass" if workload_partition in present_partitions else "fail"
                ),
                confidence_label=highest_confidence_label(confidence_labels),
                scenario_id=scenario_id,
                workload_partition=workload_partition,
                details={
                    "present_in_records": workload_partition in present_partitions,
                },
            )

    claims_by_scope: dict[str, list[dict]] = {}
    for observation in claim_observations:
        claims_by_scope.setdefault(observation["claim_scope"], []).append(observation)

    adjudication_rows: list[dict] = []
    for claim_scope in sorted(claims_by_scope):
        observations = sorted(
            claims_by_scope[claim_scope],
            key=lambda entry: (
                str(entry.get("source", "")),
                str(entry.get("claim_id", "")),
                str(entry.get("scenario_id", "")),
                str(entry.get("workload_partition", "")),
            ),
        )
        canonical_observations = [obs for obs in observations if obs.get("canonical")]
        noncanonical_observations = [
            obs for obs in observations if not obs.get("canonical")
        ]
        observed_outcomes = sorted(
            {
                str(obs.get("reported_outcome", "")).strip()
                for obs in observations
                if str(obs.get("reported_outcome", "")).strip()
                and str(obs.get("reported_outcome", "")).strip() not in {"unknown", "missing"}
            }
        )
        canonical_outcomes = sorted(
            {
                str(obs.get("reported_outcome", "")).strip()
                for obs in canonical_observations
                if str(obs.get("reported_outcome", "")).strip()
                and str(obs.get("reported_outcome", "")).strip() not in {"unknown", "missing"}
            }
        )
        conflict_detected = len(observed_outcomes) > 1
        unresolved_conflict = False
        if canonical_observations:
            if "fail" in canonical_outcomes:
                adjudicated_outcome = "fail"
                rationale = (
                    "fail-closed adjudication: canonical claim set contains a failing outcome"
                )
            elif "warn" in canonical_outcomes:
                adjudicated_outcome = "warn"
                rationale = (
                    "canonical claim set contains warning outcomes without canonical failures"
                )
            elif "pass" in canonical_outcomes:
                adjudicated_outcome = "pass"
                rationale = "canonical claim set reports pass outcomes"
            elif any(
                str(obs.get("reported_outcome", "")).strip() == "missing"
                for obs in canonical_observations
            ):
                adjudicated_outcome = "missing"
                rationale = "canonical claim set is present but marked missing/no-data"
            else:
                adjudicated_outcome = "unknown"
                rationale = "canonical claim set did not expose a normalized adjudication outcome"
        else:
            adjudicated_outcome = "unknown"
            rationale = "no canonical evidence (fresh + lineage-matched) available for adjudication"
            if conflict_detected:
                unresolved_conflict = True
                unresolved_conflicting_claim_reasons.append(
                    f"{claim_scope}: conflicting outcomes={observed_outcomes} without canonical evidence"
                )

        canonical_sources = sorted(
            {
                str(obs.get("source", "")).strip()
                for obs in canonical_observations
                if str(obs.get("source", "")).strip()
            }
        )
        stale_or_noncanonical_sources = sorted(
            {
                str(obs.get("source", "")).strip()
                for obs in noncanonical_observations
                if str(obs.get("source", "")).strip()
            }
        )
        confidence_labels = [
            str(obs.get("confidence", "")).strip()
            for obs in canonical_observations
            if str(obs.get("confidence", "")).strip() and str(obs.get("confidence", "")).strip() != "unknown"
        ]
        adjudicated_confidence = highest_confidence_label(confidence_labels)

        adjudication_rows.append(
            {
                "claim_scope": claim_scope,
                "claim_ids": sorted(
                    {
                        str(obs.get("claim_id", "")).strip()
                        for obs in observations
                        if str(obs.get("claim_id", "")).strip()
                    }
                ),
                "metric_scopes": sorted(
                    {
                        str(obs.get("metric_scope", "")).strip()
                        for obs in observations
                        if str(obs.get("metric_scope", "")).strip()
                    }
                ),
                "scenario_classes": sorted(
                    {
                        str(obs.get("scenario_class", "")).strip()
                        for obs in observations
                        if str(obs.get("scenario_class", "")).strip()
                    }
                ),
                "scenario_ids": sorted(
                    {
                        str(obs.get("scenario_id", "")).strip()
                        for obs in observations
                        if str(obs.get("scenario_id", "")).strip()
                    }
                ),
                "workload_partitions": sorted(
                    {
                        str(obs.get("workload_partition", "")).strip()
                        for obs in observations
                        if str(obs.get("workload_partition", "")).strip()
                    }
                ),
                "observed_outcomes": observed_outcomes,
                "conflict_detected": conflict_detected,
                "unresolved_conflict": unresolved_conflict,
                "adjudicated_outcome": adjudicated_outcome,
                "adjudicated_confidence": adjudicated_confidence,
                "adjudication_rationale": rationale,
                "canonical_sources": canonical_sources,
                "stale_or_noncanonical_sources": stale_or_noncanonical_sources,
                "observations": observations,
            }
        )

    adjudication_summary = {
        "total_claims": len(adjudication_rows),
        "observation_count": len(claim_observations),
        "conflict_count": sum(1 for row in adjudication_rows if row["conflict_detected"]),
        "resolved_conflict_count": sum(
            1
            for row in adjudication_rows
            if row["conflict_detected"] and not row["unresolved_conflict"]
        ),
        "unresolved_conflict_count": sum(
            1 for row in adjudication_rows if row["unresolved_conflict"]
        ),
        "pass_count": sum(1 for row in adjudication_rows if row["adjudicated_outcome"] == "pass"),
        "warn_count": sum(1 for row in adjudication_rows if row["adjudicated_outcome"] == "warn"),
        "fail_count": sum(1 for row in adjudication_rows if row["adjudicated_outcome"] == "fail"),
        "missing_count": sum(
            1 for row in adjudication_rows if row["adjudicated_outcome"] == "missing"
        ),
        "unknown_count": sum(
            1 for row in adjudication_rows if row["adjudicated_outcome"] == "unknown"
        ),
        "overall_status": (
            "pass" if not unresolved_conflicting_claim_reasons else "fail"
        ),
    }

    adjudication_matrix_payload = {
        "schema": "pi.claim_integrity.evidence_adjudication_matrix.v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "correlation_id": expected_claim_correlation_id,
        "artifact_dir": str(artifact_dir),
        "freshness_cutoff": freshness_cutoff.isoformat(),
        "adjudication_policy": {
            "canonical_evidence_rule": (
                "Evidence is canonical only when generated_at is fresh and "
                "correlation_id matches the run correlation_id."
            ),
            "conflict_resolution_rule": (
                "Fail-closed precedence applies across canonical outcomes: fail > warn > pass > missing > unknown."
            ),
            "unresolved_conflict_rule": (
                "Conflicts are unresolved when outcomes differ but no canonical evidence is available."
            ),
        },
        "source_artifacts": artifact_claim_metadata,
        "summary": adjudication_summary,
        "claims": adjudication_rows,
    }
    adjudication_matrix_json_path = (
        artifact_dir / "claim_integrity_evidence_adjudication_matrix.json"
    )
    adjudication_matrix_json_path.write_text(
        json.dumps(adjudication_matrix_payload, indent=2) + "\n",
        encoding="utf-8",
    )

    adjudication_matrix_markdown_path = (
        artifact_dir / "claim_integrity_evidence_adjudication_matrix.md"
    )
    md_lines = [
        "# Claim-Integrity Evidence Adjudication Matrix",
        "",
        f"- Schema: `{adjudication_matrix_payload['schema']}`",
        f"- Correlation ID: `{expected_claim_correlation_id}`",
        f"- Total claims: `{adjudication_summary['total_claims']}`",
        f"- Observation count: `{adjudication_summary['observation_count']}`",
        f"- Conflict count: `{adjudication_summary['conflict_count']}`",
        f"- Unresolved conflicts: `{adjudication_summary['unresolved_conflict_count']}`",
        f"- Overall status: `{adjudication_summary['overall_status']}`",
        "",
        "| Claim Scope | Outcome | Conflict | Canonical Sources | Rationale |",
        "| --- | --- | --- | --- | --- |",
    ]
    for row in adjudication_rows:
        conflict_label = (
            "UNRESOLVED"
            if row["unresolved_conflict"]
            else ("CONFLICT" if row["conflict_detected"] else "NONE")
        )
        canonical_sources_cell = ", ".join(row["canonical_sources"]) or "none"
        rationale = str(row["adjudication_rationale"]).replace("|", "\\|")
        md_lines.append(
            f"| `{row['claim_scope']}` | `{row['adjudicated_outcome']}` | `{conflict_label}` | `{canonical_sources_cell}` | {rationale} |"
        )
    adjudication_matrix_markdown_path.write_text(
        "\n".join(md_lines) + "\n",
        encoding="utf-8",
    )

    require_condition(
        "claim_integrity.evidence_adjudication_matrix_schema",
        path=adjudication_matrix_json_path,
        ok=adjudication_matrix_payload.get("schema")
        == "pi.claim_integrity.evidence_adjudication_matrix.v1",
        ok_msg="evidence-adjudication matrix schema matches",
        fail_msg=(
            "evidence-adjudication matrix schema mismatch: expected "
            "'pi.claim_integrity.evidence_adjudication_matrix.v1'"
        ),
        strict=claim_integrity_required,
    )
    add_check(
        "claim_integrity.evidence_adjudication_matrix_json",
        adjudication_matrix_json_path,
        True,
        f"evidence-adjudication matrix JSON written: {adjudication_matrix_json_path}",
    )
    add_check(
        "claim_integrity.evidence_adjudication_matrix_markdown",
        adjudication_matrix_markdown_path,
        True,
        (
            "evidence-adjudication matrix markdown written: "
            f"{adjudication_matrix_markdown_path}"
        ),
    )

franken_node_mission_contract_path = (
    project_root / "docs" / "franken-node-mission-contract.json"
)
franken_node_mission_contract = load_json(
    "claim_integrity.franken_node_mission_contract_json",
    franken_node_mission_contract_path,
    strict=True,
)
require_keys(
    "claim_integrity.franken_node_mission_contract_json",
    franken_node_mission_contract,
    franken_node_mission_contract_path,
    [
        "schema",
        "bead_id",
        "contract_version",
        "mission_statement",
        "scope_boundaries",
        "claim_tiers",
        "forbidden_claims",
        "release_claim_policy",
    ],
    strict=True,
)

franken_node_default_tier = "extension_host_dropin"
franken_node_strict_tier = "full_runtime_replacement"
franken_node_requested_tier = requested_claim_tier_raw or franken_node_default_tier
franken_node_forbidden_claim_ids: list[str] = []
extension_host_blocking_reasons: list[str] = []
strict_replacement_blocking_reasons: list[str] = []
requested_claim_tier_blocking_reasons: list[str] = []
requested_claim_tier_allowed = False
strict_verdict_required = "CERTIFIED"
strict_verdict_path = project_root / "docs" / "evidence" / "dropin-certification-verdict.json"
strict_verdict_observed = ""
strict_verdict_schema = ""
strict_verdict_schema_ok = False
strict_verdict_ready = False
strict_required_beads: list[str] = []

if isinstance(franken_node_mission_contract, dict):
    require_condition(
        "claim_integrity.franken_node_mission_contract_schema",
        path=franken_node_mission_contract_path,
        ok=str(franken_node_mission_contract.get("schema", "")).strip()
        == "pi.franken_node.mission_contract.v1",
        ok_msg="FrankenNode mission contract schema matches",
        fail_msg=(
            "FrankenNode mission contract schema must be "
            "'pi.franken_node.mission_contract.v1'"
        ),
        strict=True,
        remediation=(
            "Set docs/franken-node-mission-contract.json schema to "
            "pi.franken_node.mission_contract.v1."
        ),
    )
    require_condition(
        "claim_integrity.franken_node_mission_contract_bead_id",
        path=franken_node_mission_contract_path,
        ok=str(franken_node_mission_contract.get("bead_id", "")).strip() == "bd-3ar8v.7.1",
        ok_msg="FrankenNode mission contract bead_id is bd-3ar8v.7.1",
        fail_msg=(
            "FrankenNode mission contract bead_id must be bd-3ar8v.7.1, got "
            f"{franken_node_mission_contract.get('bead_id')!r}"
        ),
        strict=True,
    )
    contract_version = str(franken_node_mission_contract.get("contract_version", "")).strip()
    require_condition(
        "claim_integrity.franken_node_mission_contract_version_semver",
        path=franken_node_mission_contract_path,
        ok=bool(re.fullmatch(r"\d+\.\d+\.\d+", contract_version)),
        ok_msg=f"FrankenNode mission contract version is semver ({contract_version})",
        fail_msg=f"FrankenNode mission contract version must be semver, got {contract_version!r}",
        strict=True,
    )

    release_claim_policy = franken_node_mission_contract.get("release_claim_policy")
    release_claim_policy_obj = (
        release_claim_policy if isinstance(release_claim_policy, dict) else None
    )
    require_condition(
        "claim_integrity.franken_node_release_claim_policy_object",
        path=franken_node_mission_contract_path,
        ok=release_claim_policy_obj is not None,
        ok_msg="FrankenNode release_claim_policy object exists",
        fail_msg="FrankenNode release_claim_policy must be an object",
        strict=True,
    )
    if release_claim_policy_obj is not None:
        default_tier_raw = str(
            release_claim_policy_obj.get("default_allowed_tier", "")
        ).strip()
        strict_tier_raw = str(release_claim_policy_obj.get("strict_claim_tier", "")).strip()
        verdict_path_raw = str(
            release_claim_policy_obj.get("strict_claim_verdict_artifact", "")
        ).strip()
        verdict_required_raw = str(
            release_claim_policy_obj.get("strict_claim_requires_dropin_verdict", "")
        ).strip()
        if default_tier_raw:
            franken_node_default_tier = default_tier_raw
        if strict_tier_raw:
            franken_node_strict_tier = strict_tier_raw
        if verdict_path_raw:
            strict_verdict_path = project_root / verdict_path_raw
        if verdict_required_raw:
            strict_verdict_required = verdict_required_raw.upper()
        franken_node_requested_tier = requested_claim_tier_raw or franken_node_default_tier
        require_condition(
            "claim_integrity.franken_node_release_claim_policy_fail_closed",
            path=franken_node_mission_contract_path,
            ok=release_claim_policy_obj.get("fail_closed") is True,
            ok_msg="FrankenNode release claim policy is fail-closed",
            fail_msg="release_claim_policy.fail_closed must be true",
            strict=True,
        )

    claim_tiers_raw = franken_node_mission_contract.get("claim_tiers")
    claim_tiers = claim_tiers_raw if isinstance(claim_tiers_raw, list) else []
    tier_by_id: dict[str, dict] = {}
    for tier in claim_tiers:
        if not isinstance(tier, dict):
            continue
        tier_id = str(tier.get("tier_id", "")).strip()
        if not tier_id:
            continue
        tier_by_id[tier_id] = tier
    required_tier_ids = {franken_node_default_tier, franken_node_strict_tier}
    missing_tier_ids = sorted(
        tier_id for tier_id in required_tier_ids if tier_id not in tier_by_id
    )
    require_condition(
        "claim_integrity.franken_node_claim_tiers_defined",
        path=franken_node_mission_contract_path,
        ok=not missing_tier_ids,
        ok_msg=(
            "FrankenNode claim tiers include default and strict tiers "
            f"({sorted(required_tier_ids)})"
        ),
        fail_msg=f"FrankenNode claim tiers missing: {missing_tier_ids}",
        strict=True,
        remediation=(
            "Define both extension_host_dropin and full_runtime_replacement "
            "claim tiers in docs/franken-node-mission-contract.json."
        ),
    )

    extension_tier_required_checks = {
        "claim_integrity.realistic_session_shape_coverage",
        "claim_integrity.microbench_only_claim",
        "claim_integrity.global_claim_missing_partition_coverage",
        "claim_integrity.unresolved_conflicting_claims",
        "claim_integrity.evidence_adjudication_matrix_schema",
        "claim_integrity.franken_node_requested_claim_tier_allowed",
    }
    extension_required_checks_raw = (
        tier_by_id.get(franken_node_default_tier, {}).get("required_check_ids", [])
        if franken_node_default_tier in tier_by_id
        else []
    )
    extension_required_checks = {
        str(check_id).strip()
        for check_id in extension_required_checks_raw
        if str(check_id).strip()
    }
    missing_extension_required_checks = sorted(
        check_id
        for check_id in extension_tier_required_checks
        if check_id not in extension_required_checks
    )
    require_condition(
        "claim_integrity.franken_node_extension_tier_required_checks",
        path=franken_node_mission_contract_path,
        ok=not missing_extension_required_checks,
        ok_msg="FrankenNode extension_host_dropin tier required_check_ids complete",
        fail_msg=(
            "FrankenNode extension_host_dropin tier missing required checks: "
            f"{missing_extension_required_checks}"
        ),
        strict=True,
    )

    strict_required_checks_raw = (
        tier_by_id.get(franken_node_strict_tier, {}).get("required_check_ids", [])
        if franken_node_strict_tier in tier_by_id
        else []
    )
    strict_required_checks = {
        str(check_id).strip()
        for check_id in strict_required_checks_raw
        if str(check_id).strip()
    }
    require_condition(
        "claim_integrity.franken_node_strict_tier_required_checks",
        path=franken_node_mission_contract_path,
        ok=(
            "claim_integrity.franken_node_strict_replacement_dropin_certified"
            in strict_required_checks
            and "claim_integrity.franken_node_phase6_runtime_beads_declared"
            in strict_required_checks
            and "claim_integrity.franken_node_strict_tier_required_evidence"
            in strict_required_checks
        ),
        ok_msg="FrankenNode full_runtime_replacement required_check_ids include strict gates",
        fail_msg=(
            "FrankenNode full_runtime_replacement required_check_ids must include "
            "claim_integrity.franken_node_strict_replacement_dropin_certified and "
            "claim_integrity.franken_node_phase6_runtime_beads_declared and "
            "claim_integrity.franken_node_strict_tier_required_evidence"
        ),
        strict=True,
    )

    strict_required_beads_raw = (
        tier_by_id.get(franken_node_strict_tier, {}).get("required_beads", [])
        if franken_node_strict_tier in tier_by_id
        else []
    )
    strict_required_beads = [
        str(bead_id).strip()
        for bead_id in strict_required_beads_raw
        if str(bead_id).strip()
    ]
    expected_phase6_beads = {"bd-3ar8v.7.2", "bd-3ar8v.7.3", "bd-3ar8v.7.4"}
    missing_phase6_beads = sorted(
        bead_id for bead_id in expected_phase6_beads if bead_id not in strict_required_beads
    )
    require_condition(
        "claim_integrity.franken_node_phase6_runtime_beads_declared",
        path=franken_node_mission_contract_path,
        ok=not missing_phase6_beads,
        ok_msg="FrankenNode strict tier declares required phase-6 runtime beads",
        fail_msg=f"FrankenNode strict tier missing required beads: {missing_phase6_beads}",
        strict=True,
    )

    strict_required_artifacts_raw = (
        tier_by_id.get(franken_node_strict_tier, {}).get("required_evidence_artifacts", [])
        if franken_node_strict_tier in tier_by_id
        else []
    )
    strict_required_artifacts = [
        str(artifact).strip()
        for artifact in strict_required_artifacts_raw
        if str(artifact).strip()
    ]
    kernel_boundary_manifest_rel_path = "docs/franken-node-kernel-extraction-boundary-manifest.json"
    kernel_boundary_declared_report_artifact = (
        "tests/full_suite_gate/franken_node_kernel_boundary_drift_report.json"
    )
    kernel_boundary_manifest_path = project_root / kernel_boundary_manifest_rel_path
    franken_node_kernel_boundary_drift_report_json_path = (
        artifact_dir / kernel_boundary_drift_report_filename
    )
    kernel_boundary_required_checks = [
        "kernel_boundary.all_modules_mapped_or_deferred",
        "kernel_boundary.no_duplicate_domain_ownership",
        "kernel_boundary.banned_cross_boundary_pairs_absent",
        "kernel_boundary.reintegration_target_list_complete",
        "kernel_boundary.reintegration_module_mappings_complete",
    ]
    kernel_boundary_manifest = load_json(
        "claim_integrity.franken_node_kernel_boundary_manifest_json",
        kernel_boundary_manifest_path,
        strict=claim_integrity_required,
    )
    kernel_boundary_schema = ""
    kernel_boundary_schema_ok = False
    kernel_boundary_report_artifact_ok = False
    kernel_boundary_failure_policy_ok = False
    kernel_boundary_required_checks_set: set[str] = set()
    kernel_boundary_missing_required_checks = list(kernel_boundary_required_checks)
    kernel_boundary_required_checks_ok = False
    kernel_boundary_module_mappings_ok = False
    kernel_boundary_replacement_targets_count = 0
    kernel_boundary_module_mapping_count = 0
    if isinstance(kernel_boundary_manifest, dict):
        kernel_boundary_schema = str(kernel_boundary_manifest.get("schema", "")).strip()
        kernel_boundary_schema_ok = (
            kernel_boundary_schema == "pi.frankennode.kernel_extraction_boundary_manifest.v1"
        )
        require_condition(
            "claim_integrity.franken_node_kernel_boundary_manifest_schema",
            path=kernel_boundary_manifest_path,
            ok=kernel_boundary_schema_ok,
            ok_msg="kernel-boundary manifest schema matches expected contract",
            fail_msg=(
                "kernel-boundary manifest schema must be "
                "'pi.frankennode.kernel_extraction_boundary_manifest.v1'"
            ),
            strict=claim_integrity_required,
        )

        drift_detection_contract = kernel_boundary_manifest.get("drift_detection_contract")
        drift_detection_contract_obj = (
            drift_detection_contract if isinstance(drift_detection_contract, dict) else None
        )
        declared_report_artifact = (
            str(drift_detection_contract_obj.get("report_artifact", "")).strip()
            if drift_detection_contract_obj is not None
            else ""
        )
        kernel_boundary_report_artifact_ok = (
            declared_report_artifact == kernel_boundary_declared_report_artifact
        )
        require_condition(
            "claim_integrity.franken_node_kernel_boundary_report_artifact_declared",
            path=kernel_boundary_manifest_path,
            ok=kernel_boundary_report_artifact_ok,
            ok_msg="kernel-boundary manifest declares expected drift report artifact",
            fail_msg=(
                "kernel-boundary manifest drift_detection_contract.report_artifact must be "
                f"{kernel_boundary_declared_report_artifact!r} "
                f"(got {declared_report_artifact!r})"
            ),
            strict=claim_integrity_required,
        )

        declared_required_checks_raw = (
            drift_detection_contract_obj.get("required_checks", [])
            if drift_detection_contract_obj is not None
            else []
        )
        kernel_boundary_required_checks_set = {
            str(check_id).strip()
            for check_id in declared_required_checks_raw
            if str(check_id).strip()
        }
        kernel_boundary_missing_required_checks = sorted(
            check_id
            for check_id in kernel_boundary_required_checks
            if check_id not in kernel_boundary_required_checks_set
        )
        kernel_boundary_required_checks_ok = not kernel_boundary_missing_required_checks
        require_condition(
            "claim_integrity.franken_node_kernel_boundary_required_checks_declared",
            path=kernel_boundary_manifest_path,
            ok=kernel_boundary_required_checks_ok,
            ok_msg="kernel-boundary manifest declares required drift checks",
            fail_msg=(
                "kernel-boundary manifest missing required drift checks: "
                f"{kernel_boundary_missing_required_checks}"
            ),
            strict=claim_integrity_required,
        )

        drift_failure_policy = (
            str(drift_detection_contract_obj.get("failure_policy", "")).strip().lower()
            if drift_detection_contract_obj is not None
            else ""
        )
        kernel_boundary_failure_policy_ok = drift_failure_policy == "hard_fail"
        require_condition(
            "claim_integrity.franken_node_kernel_boundary_failure_policy_hard_fail",
            path=kernel_boundary_manifest_path,
            ok=kernel_boundary_failure_policy_ok,
            ok_msg="kernel-boundary drift detection contract is hard_fail",
            fail_msg=(
                "kernel-boundary drift_detection_contract.failure_policy must be hard_fail "
                f"(got {drift_failure_policy or 'missing'})"
            ),
            strict=claim_integrity_required,
        )

        reintegration_linkage = kernel_boundary_manifest.get("reintegration_linkage")
        reintegration_linkage_obj = (
            reintegration_linkage if isinstance(reintegration_linkage, dict) else None
        )
        replacement_targets_raw = (
            reintegration_linkage_obj.get("replacement_targets", [])
            if reintegration_linkage_obj is not None
            else []
        )
        module_mappings_raw = (
            reintegration_linkage_obj.get("module_mappings", [])
            if reintegration_linkage_obj is not None
            else []
        )
        require_explicit_module_mappings = (
            reintegration_linkage_obj.get("require_explicit_module_mappings") is True
            if reintegration_linkage_obj is not None
            else False
        )
        kernel_boundary_replacement_targets_count = len(
            [
                target
                for target in replacement_targets_raw
                if str(target).strip()
            ]
        )
        kernel_boundary_module_mapping_count = len(
            [entry for entry in module_mappings_raw if isinstance(entry, dict)]
        )
        kernel_boundary_module_mappings_ok = (
            require_explicit_module_mappings
            and kernel_boundary_replacement_targets_count > 0
            and kernel_boundary_module_mapping_count > 0
        )
        require_condition(
            "claim_integrity.franken_node_kernel_boundary_module_mappings_present",
            path=kernel_boundary_manifest_path,
            ok=kernel_boundary_module_mappings_ok,
            ok_msg="kernel-boundary manifest declares explicit reintegration module mappings",
            fail_msg=(
                "kernel-boundary reintegration_linkage must declare explicit non-empty "
                "replacement_targets and module_mappings"
            ),
            strict=claim_integrity_required,
        )

    kernel_boundary_check_statuses: list[dict[str, str]] = []
    for check_id in kernel_boundary_required_checks:
        declared = check_id in kernel_boundary_required_checks_set
        status = "pass" if declared else "fail"
        reason = (
            "declared in drift_detection_contract.required_checks"
            if declared
            else "missing from drift_detection_contract.required_checks"
        )
        if (
            status == "pass"
            and check_id
            in {
                "kernel_boundary.all_modules_mapped_or_deferred",
                "kernel_boundary.reintegration_target_list_complete",
                "kernel_boundary.reintegration_module_mappings_complete",
            }
            and not kernel_boundary_module_mappings_ok
        ):
            status = "fail"
            reason = "reintegration module mappings/replacement targets are incomplete"
        kernel_boundary_check_statuses.append(
            {
                "check_id": check_id,
                "status": status,
                "reason": reason,
            }
        )

    kernel_boundary_pass_count = sum(
        1 for row in kernel_boundary_check_statuses if row["status"] == "pass"
    )
    kernel_boundary_fail_count = len(kernel_boundary_check_statuses) - kernel_boundary_pass_count
    kernel_boundary_overall_status = (
        "pass"
        if (
            kernel_boundary_schema_ok
            and kernel_boundary_report_artifact_ok
            and kernel_boundary_failure_policy_ok
            and kernel_boundary_required_checks_ok
            and kernel_boundary_module_mappings_ok
            and kernel_boundary_fail_count == 0
        )
        else "fail"
    )

    franken_node_kernel_boundary_drift_report_payload = {
        "schema": "pi.franken_node.kernel_boundary_drift_report.v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "run_id": run_id,
        "correlation_id": expected_claim_correlation_id,
        "manifest": {
            "path": str(kernel_boundary_manifest_path),
            "schema": kernel_boundary_schema,
            "contract_version": (
                kernel_boundary_manifest.get("contract_version")
                if isinstance(kernel_boundary_manifest, dict)
                else None
            ),
            "support_bead_ids": (
                kernel_boundary_manifest.get("support_bead_ids", [])
                if isinstance(kernel_boundary_manifest, dict)
                else []
            ),
        },
        "checks": kernel_boundary_check_statuses,
        "summary": {
            "required_checks_count": len(kernel_boundary_required_checks),
            "passing_checks": kernel_boundary_pass_count,
            "failing_checks": kernel_boundary_fail_count,
            "replacement_targets_count": kernel_boundary_replacement_targets_count,
            "module_mappings_count": kernel_boundary_module_mapping_count,
            "overall_status": kernel_boundary_overall_status,
        },
    }
    franken_node_kernel_boundary_drift_report_json_path.parent.mkdir(
        parents=True,
        exist_ok=True,
    )
    franken_node_kernel_boundary_drift_report_json_path.write_text(
        json.dumps(franken_node_kernel_boundary_drift_report_payload, indent=2) + "\n",
        encoding="utf-8",
    )
    add_check(
        "claim_integrity.franken_node_kernel_boundary_drift_report_json",
        franken_node_kernel_boundary_drift_report_json_path,
        True,
        "kernel-boundary drift report JSON written",
    )
    require_condition(
        "claim_integrity.franken_node_kernel_boundary_drift_report_schema",
        path=franken_node_kernel_boundary_drift_report_json_path,
        ok=(
            franken_node_kernel_boundary_drift_report_payload.get("schema")
            == "pi.franken_node.kernel_boundary_drift_report.v1"
        ),
        ok_msg="kernel-boundary drift report schema matches",
        fail_msg=(
            "kernel-boundary drift report schema must be "
            "'pi.franken_node.kernel_boundary_drift_report.v1'"
        ),
        strict=claim_integrity_required,
    )
    require_condition(
        "claim_integrity.franken_node_kernel_boundary_drift_report_pass",
        path=franken_node_kernel_boundary_drift_report_json_path,
        ok=kernel_boundary_overall_status == "pass",
        ok_msg="kernel-boundary drift report overall_status is pass",
        fail_msg=(
            "kernel-boundary drift report overall_status is fail; "
            f"missing checks={kernel_boundary_missing_required_checks}"
        ),
        strict=claim_integrity_required,
    )
    if kernel_boundary_overall_status != "pass":
        strict_replacement_blocking_reasons.append(
            "kernel-boundary drift report indicates unresolved extraction drift checks"
        )

    missing_strict_required_artifacts = sorted(
        artifact
        for artifact in strict_required_artifacts
        if not (project_root / artifact).exists()
    )
    strict_tier_required_evidence_ready = not missing_strict_required_artifacts
    if missing_strict_required_artifacts:
        strict_replacement_blocking_reasons.append(
            "strict tier required evidence artifacts missing: "
            f"{missing_strict_required_artifacts}"
        )
    require_condition(
        "claim_integrity.franken_node_strict_tier_required_evidence",
        path=franken_node_mission_contract_path,
        ok=strict_tier_required_evidence_ready,
        ok_msg="FrankenNode strict tier required evidence artifacts are present",
        fail_msg=(
            "FrankenNode strict tier missing required evidence artifacts: "
            f"{missing_strict_required_artifacts}"
        ),
        strict=claim_integrity_required,
    )

    forbidden_claims_raw = franken_node_mission_contract.get("forbidden_claims")
    forbidden_claims = forbidden_claims_raw if isinstance(forbidden_claims_raw, list) else []
    strict_forbidden_claim_ids: list[str] = []
    strict_forbidden_phrases: list[str] = []
    for claim in forbidden_claims:
        if not isinstance(claim, dict):
            continue
        claim_id = str(claim.get("claim_id", "")).strip()
        phrase = str(claim.get("phrase", "")).strip()
        blocked_until_tier = str(claim.get("blocked_until_tier", "")).strip()
        if blocked_until_tier == franken_node_strict_tier:
            if claim_id:
                strict_forbidden_claim_ids.append(claim_id)
            if phrase:
                strict_forbidden_phrases.append(phrase.lower())
    franken_node_forbidden_claim_ids = sorted(set(strict_forbidden_claim_ids))
    require_condition(
        "claim_integrity.franken_node_forbidden_claims_defined",
        path=franken_node_mission_contract_path,
        ok=bool(franken_node_forbidden_claim_ids),
        ok_msg=(
            "FrankenNode mission contract declares strict-tier forbidden claims "
            f"({franken_node_forbidden_claim_ids})"
        ),
        fail_msg="FrankenNode mission contract must declare forbidden strict-tier claims",
        strict=True,
    )
    strict_language_coverage_ok = any("node" in phrase for phrase in strict_forbidden_phrases) and any(
        "bun" in phrase for phrase in strict_forbidden_phrases
    )
    require_condition(
        "claim_integrity.franken_node_forbidden_claim_language_coverage",
        path=franken_node_mission_contract_path,
        ok=strict_language_coverage_ok,
        ok_msg="forbidden strict-tier claims cover both Node and Bun language",
        fail_msg=(
            "forbidden strict-tier claims must include explicit Node and Bun language "
            "patterns"
        ),
        strict=True,
    )

    release_overall_ready = (
        release_readiness.get("overall_ready")
        if isinstance(release_readiness, dict)
        else None
    )
    if not claim_integrity_gate_active:
        extension_host_blocking_reasons.append(
            "claim_integrity gate inactive; cannot certify extension_host_dropin tier"
        )
    if evidence_missing_or_stale_reasons:
        extension_host_blocking_reasons.extend(evidence_missing_or_stale_reasons)
    if microbench_only_claim_reasons:
        extension_host_blocking_reasons.extend(microbench_only_claim_reasons)
    if global_claim_partition_reasons:
        extension_host_blocking_reasons.extend(global_claim_partition_reasons)
    if unresolved_conflicting_claim_reasons:
        extension_host_blocking_reasons.extend(unresolved_conflicting_claim_reasons)
    scenario_overall_status = ""
    if isinstance(scenario_cell_status_payload, dict):
        scenario_summary = scenario_cell_status_payload.get("summary", {})
        if isinstance(scenario_summary, dict):
            scenario_overall_status = normalize_claim_outcome(
                scenario_summary.get("overall_status")
            )
    if scenario_overall_status != "pass":
        extension_host_blocking_reasons.append(
            f"scenario cell status overall_status must be pass (got {scenario_overall_status or 'missing'})"
        )
    adjudication_overall_status = ""
    if isinstance(adjudication_matrix_payload, dict):
        adjudication_summary_obj = adjudication_matrix_payload.get("summary", {})
        if isinstance(adjudication_summary_obj, dict):
            adjudication_overall_status = normalize_claim_outcome(
                adjudication_summary_obj.get("overall_status")
            )
    if adjudication_overall_status != "pass":
        extension_host_blocking_reasons.append(
            "evidence adjudication matrix overall_status must be pass "
            f"(got {adjudication_overall_status or 'missing'})"
        )
    if release_overall_ready is not True:
        extension_host_blocking_reasons.append(
            f"release_readiness.overall_ready must be true (got {release_overall_ready!r})"
        )
    extension_host_blocking_reasons = sorted(set(extension_host_blocking_reasons))
    extension_host_tier_ready = not extension_host_blocking_reasons
    require_condition(
        "claim_integrity.franken_node_extension_host_tier_evidence",
        path=franken_node_mission_contract_path,
        ok=extension_host_tier_ready,
        ok_msg="extension_host_dropin tier evidence is sufficient",
        fail_msg=(
            "extension_host_dropin tier evidence is insufficient: "
            f"{extension_host_blocking_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Regenerate full claim-integrity evidence bundle and resolve blocking "
            "claim-integrity checks before release-facing extension-host claims."
        ),
    )

    strict_verdict_payload = None
    if strict_verdict_path.exists():
        try:
            strict_verdict_payload = json.loads(strict_verdict_path.read_text(encoding="utf-8"))
        except Exception as exc:  # pragma: no cover - defensive
            strict_replacement_blocking_reasons.append(
                f"strict replacement verdict artifact invalid JSON ({exc})"
            )
    else:
        strict_replacement_blocking_reasons.append(
            f"strict replacement verdict artifact missing: {strict_verdict_path}"
        )
    if isinstance(strict_verdict_payload, dict):
        strict_verdict_schema = str(strict_verdict_payload.get("schema", "")).strip()
        strict_verdict_schema_ok = (
            strict_verdict_schema == "pi.dropin.certification_verdict.v1"
        )
        strict_verdict_observed = str(
            strict_verdict_payload.get("overall_verdict", "")
        ).strip()
        if not strict_verdict_observed:
            strict_verdict_observed = str(strict_verdict_payload.get("verdict", "")).strip()
        if not strict_verdict_schema_ok:
            strict_replacement_blocking_reasons.append(
                "strict replacement verdict schema must be pi.dropin.certification_verdict.v1"
            )
        if strict_verdict_observed.upper() != strict_verdict_required:
            strict_replacement_blocking_reasons.append(
                "strict replacement verdict must be "
                f"{strict_verdict_required} (got {strict_verdict_observed or 'missing'})"
            )
    strict_verdict_ready = (
        strict_verdict_schema_ok and strict_verdict_observed.upper() == strict_verdict_required
    )
    strict_replacement_blocking_reasons = sorted(set(strict_replacement_blocking_reasons))
    strict_replacement_tier_ready = (
        extension_host_tier_ready
        and strict_verdict_ready
        and not missing_phase6_beads
        and strict_tier_required_evidence_ready
    )
    require_condition(
        "claim_integrity.franken_node_strict_replacement_dropin_certified",
        path=strict_verdict_path,
        ok=strict_verdict_ready,
        ok_msg=(
            "strict replacement verdict artifact is certified "
            f"({strict_verdict_required})"
        ),
        fail_msg=(
            "strict replacement verdict artifact is not certified: "
            f"{strict_replacement_blocking_reasons}"
        ),
        strict=False,
        remediation=(
            "Generate docs/evidence/dropin-certification-verdict.json with "
            "overall_verdict=CERTIFIED before enabling strict runtime-replacement claims."
        ),
    )

    known_tier_ids = sorted(tier_by_id.keys())
    requested_tier_known = franken_node_requested_tier in tier_by_id
    require_condition(
        "claim_integrity.franken_node_requested_claim_tier_known",
        path=franken_node_mission_contract_path,
        ok=requested_tier_known,
        ok_msg=f"requested claim tier is known ({franken_node_requested_tier})",
        fail_msg=(
            f"requested claim tier {franken_node_requested_tier!r} is unknown; "
            f"known tiers={known_tier_ids}"
        ),
        strict=claim_integrity_required,
    )
    if requested_tier_known and franken_node_requested_tier == franken_node_default_tier:
        requested_claim_tier_allowed = extension_host_tier_ready
        requested_claim_tier_blocking_reasons.extend(extension_host_blocking_reasons)
    elif requested_tier_known and franken_node_requested_tier == franken_node_strict_tier:
        requested_claim_tier_allowed = strict_replacement_tier_ready
        requested_claim_tier_blocking_reasons.extend(strict_replacement_blocking_reasons)
        if not strict_replacement_tier_ready and franken_node_forbidden_claim_ids:
            requested_claim_tier_blocking_reasons.append(
                "forbidden strict-tier claim IDs still blocked: "
                f"{franken_node_forbidden_claim_ids}"
            )
        if not extension_host_tier_ready:
            requested_claim_tier_blocking_reasons.append(
                "extension_host_dropin tier must pass before strict tier claims"
            )
    else:
        requested_claim_tier_allowed = False
        requested_claim_tier_blocking_reasons.append(
            f"requested claim tier {franken_node_requested_tier!r} is not defined by mission contract"
        )
    requested_claim_tier_blocking_reasons = sorted(set(requested_claim_tier_blocking_reasons))

    require_condition(
        "claim_integrity.franken_node_requested_claim_tier_allowed",
        path=franken_node_mission_contract_path,
        ok=requested_claim_tier_allowed,
        ok_msg=(
            "requested FrankenNode claim tier is allowed: "
            f"{franken_node_requested_tier}"
        ),
        fail_msg=(
            "requested FrankenNode claim tier is blocked: "
            f"{requested_claim_tier_blocking_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Select a lower claim tier or regenerate missing claim-integrity evidence "
            "until mission-contract gating conditions are satisfied."
        ),
    )

    franken_node_claim_gate_status_payload = {
        "schema": "pi.franken_node.claim_gate_status.v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "correlation_id": expected_claim_correlation_id,
        "mission_contract": {
            "schema": franken_node_mission_contract.get("schema"),
            "path": str(franken_node_mission_contract_path),
            "version": franken_node_mission_contract.get("contract_version"),
            "bead_id": franken_node_mission_contract.get("bead_id"),
        },
        "requested_claim_tier": franken_node_requested_tier,
        "default_claim_tier": franken_node_default_tier,
        "strict_claim_tier": franken_node_strict_tier,
        "requested_tier_allowed": requested_claim_tier_allowed,
        "requested_tier_blocking_reasons": requested_claim_tier_blocking_reasons,
        "forbidden_claim_ids": franken_node_forbidden_claim_ids,
        "tiers": {
            franken_node_default_tier: {
                "ready": extension_host_tier_ready,
                "blocking_reasons": extension_host_blocking_reasons,
            },
            franken_node_strict_tier: {
                "ready": strict_replacement_tier_ready,
                "blocking_reasons": strict_replacement_blocking_reasons,
                "required_beads": strict_required_beads,
            },
        },
        "strict_replacement_verdict": {
            "required_overall_verdict": strict_verdict_required,
            "path": str(strict_verdict_path),
            "schema": strict_verdict_schema or None,
            "schema_ok": strict_verdict_schema_ok,
            "observed_overall_verdict": strict_verdict_observed or None,
            "ready": strict_verdict_ready,
        },
    }
    franken_node_claim_gate_status_json_path = (
        artifact_dir / "franken_node_claim_gate_status.json"
    )
    franken_node_claim_gate_status_json_path.write_text(
        json.dumps(franken_node_claim_gate_status_payload, indent=2) + "\n",
        encoding="utf-8",
    )
    add_check(
        "claim_integrity.franken_node_claim_gate_status_json",
        franken_node_claim_gate_status_json_path,
        True,
        (
            "FrankenNode claim-gate status JSON written: "
            f"{franken_node_claim_gate_status_json_path}"
        ),
    )

if claim_integrity_gate_active:
    require_condition(
        "claim_integrity.missing_or_stale_evidence",
        path=perf_sli_matrix_path,
        ok=not evidence_missing_or_stale_reasons,
        ok_msg="all claim-evidence artifacts are present, fresh, and lineage-matched",
        fail_msg=(
            "missing/stale claim evidence detected: "
            f"{evidence_missing_or_stale_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Generate fresh perf evidence in CI using "
            "./scripts/perf/orchestrate.sh --profile ci with shared CI_CORRELATION_ID."
        ),
    )
    require_condition(
        "claim_integrity.missing_required_result_field",
        path=perf_sli_matrix_path,
        ok=not missing_required_result_field_reasons,
        ok_msg="required result fields are present in baseline confidence records",
        fail_msg=(
            "missing required result fields in perf evidence: "
            f"{missing_required_result_field_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Emit all required record fields in baseline_variance_confidence.json "
            "before publishing certification claims."
        ),
    )
    require_condition(
        "claim_integrity.missing_workload_partition_tag",
        path=perf_sli_matrix_path,
        ok=not missing_workload_partition_tag_reasons,
        ok_msg="all perf evidence rows include valid workload_partition tags",
        fail_msg=(
            "missing/invalid workload_partition tags detected: "
            f"{missing_workload_partition_tag_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Ensure each perf evidence row declares matched-state/realistic "
            "workload_partition values."
        ),
    )
    require_condition(
        "claim_integrity.missing_scenario_metadata",
        path=perf_sli_matrix_path,
        ok=not missing_scenario_metadata_reasons,
        ok_msg="scenario_metadata payloads satisfy reporting contract",
        fail_msg=(
            "missing/invalid scenario_metadata detected: "
            f"{missing_scenario_metadata_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Populate scenario_metadata with workflow_id/workflow_class/suite_ids/"
            "vcr_mode/scenario_owner for each perf evidence row."
        ),
    )
    require_condition(
        "claim_integrity.realistic_session_shape_coverage",
        path=realistic_shape_coverage_path or perf_sli_matrix_path,
        ok=not missing_realistic_session_shape_reasons,
        ok_msg=(
            "required realistic session-shape tiers are covered "
            f"(source={realistic_session_shape_coverage_source})"
        ),
        fail_msg=(
            "missing/invalid realistic_session_shape coverage detected: "
            f"{missing_realistic_session_shape_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Publish phase1_matrix_validation.json with realistic matrix cells "
            "(100k..5m) under PERF_EVIDENCE_DIR/results and ensure session_messages "
            "map to benchmark_partitions.realistic_long_session."
        ),
    )
    require_condition(
        "claim_integrity.invalid_evidence_class",
        path=perf_sli_matrix_path,
        ok=not invalid_evidence_class_reasons,
        ok_msg=f"evidence class labels constrained to {allowed_evidence_class}",
        fail_msg=(
            "invalid evidence_class/evidence_state labels detected: "
            f"{invalid_evidence_class_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Use only allowed evidence labels from docs/perf_sli_matrix.json "
            "ci_enforcement.allowed_evidence_class."
        ),
    )
    require_condition(
        "claim_integrity.invalid_confidence_label",
        path=perf_sli_matrix_path,
        ok=not invalid_confidence_label_reasons,
        ok_msg=f"confidence labels constrained to {allowed_confidence}",
        fail_msg=(
            "invalid confidence labels detected: "
            f"{invalid_confidence_label_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Use only allowed confidence labels from docs/perf_sli_matrix.json "
            "ci_enforcement.allowed_confidence."
        ),
    )
    require_condition(
        "claim_integrity.missing_absolute_or_relative_values",
        path=perf_sli_matrix_path,
        ok=not missing_absolute_or_relative_reasons,
        ok_msg="all required stratification layers include absolute + relative values",
        fail_msg=(
            "missing absolute/relative values detected in stratification: "
            f"{missing_absolute_or_relative_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Populate absolute metric values and Rust-vs-Node/Bun ratios for "
            "cold_load_init, per_call_dispatch_micro, and full_e2e_long_session."
        ),
    )
    require_condition(
        "claim_integrity.microbench_only_claim",
        path=perf_sli_matrix_path,
        ok=not microbench_only_claim_reasons,
        ok_msg="global claim is not microbench-only",
        fail_msg=(
            "microbench-only claim violations detected: "
            f"{microbench_only_claim_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Require full_e2e_long_session coverage and set cherry_pick_guard."
            "global_claim_valid=true before claiming global wins."
        ),
    )
    require_condition(
        "claim_integrity.global_claim_missing_partition_coverage",
        path=perf_sli_matrix_path,
        ok=not global_claim_partition_reasons,
        ok_msg="global claims include required matched-state and realistic coverage",
        fail_msg=(
            "global claim partition coverage violations detected: "
            f"{global_claim_partition_reasons}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Publish matched-state and realistic rows for every required scenario "
            "before release-facing global claims."
        ),
    )
    require_condition(
        "claim_integrity.unresolved_conflicting_claims",
        path=adjudication_matrix_json_path or perf_sli_matrix_path,
        ok=not unresolved_conflicting_claim_reasons,
        ok_msg="all conflicting claims have canonical adjudications",
        fail_msg=(
            "unresolved conflicting claims detected in evidence-adjudication matrix: "
            f"{sorted(set(unresolved_conflicting_claim_reasons))}"
        ),
        strict=claim_integrity_required,
        remediation=(
            "Regenerate baseline_variance_confidence.json, "
            "extension_benchmark_stratification.json, and "
            "phase1_matrix_validation.json with shared CI_CORRELATION_ID and fresh "
            "generated_at timestamps before release-facing claims."
        ),
    )

# 6b) Kernel-boundary drift report validation
kernel_boundary_manifest_path = (
    project_root / "docs" / "franken-node-kernel-extraction-boundary-manifest.json"
)
kernel_boundary_drift_report_path = (
    artifact_dir / kernel_boundary_drift_report_filename
)
kernel_boundary_manifest = load_json(
    "claim_integrity.kernel_boundary_manifest_json",
    kernel_boundary_manifest_path,
    strict=True,
)
kernel_boundary_drift_report = load_json(
    "claim_integrity.kernel_boundary_drift_report_json",
    kernel_boundary_drift_report_path,
    strict=True,
)
if isinstance(kernel_boundary_drift_report, dict):
    require_condition(
        "claim_integrity.kernel_boundary_drift_report_schema",
        path=kernel_boundary_drift_report_path,
        ok=(
            kernel_boundary_drift_report.get("schema")
            == "pi.franken_node.kernel_boundary_drift_report.v1"
        ),
        ok_msg="kernel-boundary drift report schema is correct",
        fail_msg=(
            "kernel-boundary drift report has wrong schema: "
            f"{kernel_boundary_drift_report.get('schema')!r}"
        ),
        strict=True,
    )
    drift_summary = kernel_boundary_drift_report.get("summary", {})
    require_condition(
        "claim_integrity.kernel_boundary_drift_report_overall_status_pass",
        path=kernel_boundary_drift_report_path,
        ok=drift_summary.get("overall_status") == "pass",
        ok_msg="kernel-boundary drift report overall_status is pass",
        fail_msg=(
            "kernel-boundary drift report overall_status is not pass: "
            f"{drift_summary.get('overall_status')!r}; "
            f"failing_checks={drift_summary.get('failing_checks', '?')}"
        ),
        strict=True,
        remediation=(
            "Rerun scripts/e2e/run_all.sh so the kernel-boundary checks regenerate "
            "the run-specific artifact."
        ),
    )
    drift_checks = kernel_boundary_drift_report.get("checks", [])
    required_drift_check_ids = {
        "kernel_boundary.all_modules_mapped_or_deferred",
        "kernel_boundary.no_duplicate_domain_ownership",
        "kernel_boundary.banned_cross_boundary_pairs_absent",
        "kernel_boundary.reintegration_target_list_complete",
        "kernel_boundary.reintegration_module_mappings_complete",
    }
    found_drift_check_ids = {
        c.get("check_id") for c in drift_checks if isinstance(c, dict)
    }
    missing_drift_checks = required_drift_check_ids - found_drift_check_ids
    require_condition(
        "claim_integrity.kernel_boundary_drift_report_checks_complete",
        path=kernel_boundary_drift_report_path,
        ok=not missing_drift_checks,
        ok_msg=f"all {len(required_drift_check_ids)} required drift checks present",
        fail_msg=f"missing required drift checks: {sorted(missing_drift_checks)}",
        strict=True,
    )


# 7) Exception-policy coverage for non-pass conformance outcomes
full_conformance_report_path = reports_dir / "conformance" / "conformance_report.json"
full_conformance_report = load_json(
    "conformance.full_report_json",
    full_conformance_report_path,
    strict=strict_conformance,
)

baseline_path = reports_dir / "conformance_baseline.json"
baseline_payload = load_json(
    "conformance.baseline_json",
    baseline_path,
    strict=strict_conformance,
)

approved_exception_ids: set[str] = set()

if isinstance(baseline_payload, dict):
    exception_policy = baseline_payload.get("exception_policy")
    require_condition(
        "conformance.exception_policy.exists",
        path=baseline_path,
        ok=isinstance(exception_policy, dict),
        ok_msg="exception_policy object exists",
        fail_msg="missing exception_policy object in conformance_baseline.json",
        strict=strict_conformance,
    )

    if isinstance(exception_policy, dict):
        require_keys(
            "conformance.exception_policy",
            exception_policy,
            baseline_path,
            ["schema", "required_fields", "entries"],
            strict=strict_conformance,
        )

        policy_schema = exception_policy.get("schema")
        schema_ok = policy_schema == "pi.ext.exception_policy.v1"
        add_check(
            "conformance.exception_policy.schema",
            baseline_path,
            schema_ok,
            f"schema={policy_schema!r}"
            if schema_ok
            else f"unexpected schema {policy_schema!r}",
        )
        if not schema_ok:
            message = (
                "conformance.exception_policy.schema: expected "
                f"'pi.ext.exception_policy.v1', got {policy_schema!r}"
            )
            if strict_conformance:
                errors.append(message)
            else:
                warnings.append(message)

        required_fields = exception_policy.get("required_fields")
        required_fields_ok = (
            isinstance(required_fields, list)
            and all(isinstance(field, str) and field for field in required_fields)
        )
        require_condition(
            "conformance.exception_policy.required_fields",
            path=baseline_path,
            ok=required_fields_ok,
            ok_msg="required_fields is a non-empty string list",
            fail_msg="required_fields must be a list[str]",
            strict=strict_conformance,
        )

        entries = exception_policy.get("entries")
        entries_ok = isinstance(entries, list)
        require_condition(
            "conformance.exception_policy.entries_array",
            path=baseline_path,
            ok=entries_ok,
            ok_msg="entries is an array",
            fail_msg="entries must be an array",
            strict=strict_conformance,
        )

        if required_fields_ok and entries_ok:
            seen_ids: set[str] = set()
            today = datetime.now(timezone.utc).date()

            for idx, entry in enumerate(entries):
                check_prefix = f"conformance.exception_policy.entry[{idx}]"
                if not isinstance(entry, dict):
                    require_condition(
                        f"{check_prefix}.object",
                        path=baseline_path,
                        ok=False,
                        ok_msg="entry is object",
                        fail_msg="entry must be an object",
                        strict=strict_conformance,
                    )
                    continue

                missing_entry_fields = [
                    field for field in required_fields if field not in entry
                ]
                require_condition(
                    f"{check_prefix}.required_fields",
                    path=baseline_path,
                    ok=not missing_entry_fields,
                    ok_msg="entry contains all required fields",
                    fail_msg=f"entry missing required fields: {missing_entry_fields}",
                    strict=strict_conformance,
                )

                entry_id = str(entry.get("id", "")).strip()
                status_value = str(entry.get("status", "")).strip()
                review_by = str(entry.get("review_by", "")).strip()

                id_ok = bool(entry_id)
                require_condition(
                    f"{check_prefix}.id",
                    path=baseline_path,
                    ok=id_ok,
                    ok_msg=f"id={entry_id}",
                    fail_msg="id must be non-empty",
                    strict=strict_conformance,
                )

                if id_ok:
                    duplicate = entry_id in seen_ids
                    require_condition(
                        f"{check_prefix}.unique_id",
                        path=baseline_path,
                        ok=not duplicate,
                        ok_msg=f"id {entry_id} is unique",
                        fail_msg=f"duplicate exception id: {entry_id}",
                        strict=strict_conformance,
                    )
                    seen_ids.add(entry_id)

                status_ok = status_value in {"approved", "temporary"}
                require_condition(
                    f"{check_prefix}.status",
                    path=baseline_path,
                    ok=status_ok,
                    ok_msg=f"status={status_value}",
                    fail_msg=(
                        f"invalid status {status_value!r}; expected approved|temporary"
                    ),
                    strict=strict_conformance,
                )

                review_ok = False
                review_error = ""
                if review_by:
                    try:
                        review_date = datetime.strptime(review_by, "%Y-%m-%d").date()
                        review_ok = review_date >= today
                        if not review_ok:
                            review_error = (
                                f"review_by {review_by} is in the past (today={today})"
                            )
                    except ValueError:
                        review_error = f"review_by {review_by!r} is not YYYY-MM-DD"
                else:
                    review_error = "review_by is missing or empty"

                require_condition(
                    f"{check_prefix}.review_by",
                    path=baseline_path,
                    ok=review_ok,
                    ok_msg=f"review_by={review_by}",
                    fail_msg=review_error,
                    strict=strict_conformance,
                )

                if (
                    id_ok
                    and status_ok
                    and review_ok
                    and not missing_entry_fields
                ):
                    approved_exception_ids.add(entry_id)

if isinstance(full_conformance_report, dict):
    failures = full_conformance_report.get("failures")
    failures_ok = isinstance(failures, list)
    require_condition(
        "conformance.full_report_json.failures_array",
        path=full_conformance_report_path,
        ok=failures_ok,
        ok_msg="full report failures field is an array",
        fail_msg="full report failures field missing or not an array",
        strict=strict_conformance,
    )

    if failures_ok:
        failure_ids = sorted(
            {
                str(item.get("id", "")).strip()
                for item in failures
                if isinstance(item, dict) and str(item.get("id", "")).strip()
            }
        )
        missing_ids = sorted(
            [entry_id for entry_id in failure_ids if entry_id not in approved_exception_ids]
        )
        require_condition(
            "conformance.exception_policy.covers_failures",
            path=full_conformance_report_path,
            ok=not missing_ids,
            ok_msg=(
                f"all {len(failure_ids)} full-report failures covered by approved exceptions"
            ),
            fail_msg=f"missing approved exceptions for failures: {missing_ids}",
            strict=strict_conformance,
        )


contract_correlation_id = summary_correlation_id or environment_correlation_id
require_condition(
    "contract.source_commit_matches_run",
    path=contract_file,
    ok=source_commit_ok,
    ok_msg="contract source_commit is bound to the captured run source",
    fail_msg=f"contract cannot bind malformed source_commit: {source_commit!r}",
    strict=True,
    remediation="Rerun from a canonical Git checkout with a valid HEAD commit.",
)
require_condition(
    "contract.source_snapshot_matches_run",
    path=contract_file,
    ok=source_snapshot_ok,
    ok_msg="contract source_snapshot is bound to the captured run source",
    fail_msg=f"contract cannot bind malformed source_snapshot: {source_snapshot!r}",
    strict=True,
    remediation="Rerun so the producer can capture a clean immutable source snapshot.",
)
status = "pass" if not errors else "fail"
contract_payload = {
    "schema": "pi.evidence.contract.v1",
    "generated_at": generated_at,
    "profile": profile,
    "artifact_dir": str(artifact_dir),
    "correlation_id": contract_correlation_id,
    "source_commit": source_commit,
    "source_snapshot": source_snapshot,
    "runner_outcome": (
        {
            "schema": runner_outcome.get("schema"),
            "path": str(runner_outcome_path),
            "status": runner_outcome.get("status"),
            "exit_code": runner_outcome.get("exit_code"),
        }
        if isinstance(runner_outcome, dict)
        else None
    ),
    "status": status,
    "strict_conformance": strict_conformance,
    "checks": checks,
    "errors": errors,
    "warnings": warnings,
    "remediation_hints": sorted(remediation_hints),
    "claim_integrity_scenario_cells": (
        {
            "schema": "pi.claim_integrity.scenario_cell_status.v1",
            "path": str(scenario_cell_status_json_path),
            "markdown_path": str(scenario_cell_status_markdown_path)
            if scenario_cell_status_markdown_path is not None
            else None,
            "summary": (
                scenario_cell_status_payload.get("summary", {})
                if isinstance(scenario_cell_status_payload, dict)
                else {}
            ),
        }
        if scenario_cell_status_json_path is not None
        else None
    ),
    "claim_integrity_adjudication_matrix": (
        {
            "schema": "pi.claim_integrity.evidence_adjudication_matrix.v1",
            "path": str(adjudication_matrix_json_path),
            "markdown_path": str(adjudication_matrix_markdown_path)
            if adjudication_matrix_markdown_path is not None
            else None,
            "summary": (
                adjudication_matrix_payload.get("summary", {})
                if isinstance(adjudication_matrix_payload, dict)
                else {}
            ),
        }
        if adjudication_matrix_json_path is not None
        else None
    ),
    "franken_node_claim_gate_status": (
        {
            "schema": "pi.franken_node.claim_gate_status.v1",
            "path": str(franken_node_claim_gate_status_json_path),
            "requested_claim_tier": (
                franken_node_claim_gate_status_payload.get("requested_claim_tier")
                if isinstance(franken_node_claim_gate_status_payload, dict)
                else None
            ),
            "requested_tier_allowed": (
                franken_node_claim_gate_status_payload.get("requested_tier_allowed")
                if isinstance(franken_node_claim_gate_status_payload, dict)
                else None
            ),
            "requested_tier_blocking_reasons": (
                franken_node_claim_gate_status_payload.get(
                    "requested_tier_blocking_reasons", []
                )
                if isinstance(franken_node_claim_gate_status_payload, dict)
                else []
            ),
            "tiers": (
                franken_node_claim_gate_status_payload.get("tiers", {})
                if isinstance(franken_node_claim_gate_status_payload, dict)
                else {}
            ),
        }
        if franken_node_claim_gate_status_json_path is not None
        else None
    ),
    "franken_node_kernel_boundary_drift_report": (
        {
            "schema": "pi.franken_node.kernel_boundary_drift_report.v1",
            "path": str(franken_node_kernel_boundary_drift_report_json_path),
            "summary": (
                franken_node_kernel_boundary_drift_report_payload.get("summary", {})
                if isinstance(franken_node_kernel_boundary_drift_report_payload, dict)
                else {}
            ),
        }
        if franken_node_kernel_boundary_drift_report_json_path is not None
        else None
    ),
}
contract_file.parent.mkdir(parents=True, exist_ok=True)
contract_json = json.dumps(contract_payload, indent=2) + "\n"
if status != "pass":
    # On a failed run, invalidate any earlier passing contract before touching
    # secondary metadata. This is the fail-closed ordering.
    contract_file.write_text(contract_json, encoding="utf-8")

if isinstance(summary, dict):
    summary["evidence_contract"] = {
        "schema": "pi.evidence.contract.v1",
        "correlation_id": contract_correlation_id,
        "source_commit": source_commit,
        "source_snapshot": source_snapshot,
        "path": str(contract_file),
        "status": status,
        "strict_conformance": strict_conformance,
        "error_count": len(errors),
        "warning_count": len(warnings),
    }
    if scenario_cell_status_json_path is not None and isinstance(
        scenario_cell_status_payload, dict
    ):
        summary["claim_integrity_scenario_cells"] = {
            "schema": "pi.claim_integrity.scenario_cell_status.v1",
            "path": str(scenario_cell_status_json_path),
            "markdown_path": str(scenario_cell_status_markdown_path)
            if scenario_cell_status_markdown_path is not None
            else None,
            "summary": scenario_cell_status_payload.get("summary", {}),
        }
    if adjudication_matrix_json_path is not None and isinstance(
        adjudication_matrix_payload, dict
    ):
        summary["claim_integrity_adjudication_matrix"] = {
            "schema": "pi.claim_integrity.evidence_adjudication_matrix.v1",
            "path": str(adjudication_matrix_json_path),
            "markdown_path": str(adjudication_matrix_markdown_path)
            if adjudication_matrix_markdown_path is not None
            else None,
            "summary": adjudication_matrix_payload.get("summary", {}),
        }
    if franken_node_claim_gate_status_json_path is not None and isinstance(
        franken_node_claim_gate_status_payload, dict
    ):
        summary["franken_node_claim_gate_status"] = {
            "schema": "pi.franken_node.claim_gate_status.v1",
            "path": str(franken_node_claim_gate_status_json_path),
            "requested_claim_tier": franken_node_claim_gate_status_payload.get(
                "requested_claim_tier"
            ),
            "requested_tier_allowed": franken_node_claim_gate_status_payload.get(
                "requested_tier_allowed"
            ),
            "requested_tier_blocking_reasons": franken_node_claim_gate_status_payload.get(
                "requested_tier_blocking_reasons",
                [],
            ),
            "tiers": franken_node_claim_gate_status_payload.get("tiers", {}),
        }
    if franken_node_kernel_boundary_drift_report_json_path is not None and isinstance(
        franken_node_kernel_boundary_drift_report_payload, dict
    ):
        summary["franken_node_kernel_boundary_drift_report"] = {
            "schema": "pi.franken_node.kernel_boundary_drift_report.v1",
            "path": str(franken_node_kernel_boundary_drift_report_json_path),
            "summary": franken_node_kernel_boundary_drift_report_payload.get(
                "summary",
                {},
            ),
        }
    summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

if status == "pass":
    # Publish a passing contract only after every summary update succeeded.
    contract_file.write_text(contract_json, encoding="utf-8")

if errors:
    print("EVIDENCE CONTRACT FAILED")
    for error in errors:
        print(f"- {error}")
    if remediation_hints:
        print("\nREMEDIATION HINTS")
        for hint in sorted(remediation_hints):
            print(f"  - {hint}")
    if warnings:
        print("\nEVIDENCE CONTRACT WARNINGS")
        for warning in warnings:
            print(f"  - {warning}")
    sys.exit(1)

print(
    "EVIDENCE CONTRACT PASSED: "
    f"{len(checks)} checks, {len(warnings)} warning(s), profile={profile}, strict_conformance={strict_conformance}"
)
sys.exit(0)
PY
    then
        echo "[contract] PASS ($contract_file)"
        return 0
    else
        echo "[contract] FAIL (see $contract_file)" >&2
        return 1
    fi
}

# ─── Main ─────────────────────────────────────────────────────────────────────

main() {
    echo "═══════════════════════════════════════════════════════════════"
    echo " Pi Agent Rust — Unified Verification Runner"
    echo " Timestamp: $TIMESTAMP"
    echo " Profile: $PROFILE"
    echo " Artifact dir: $ARTIFACT_DIR"
    echo " Lint: $(if $SKIP_LINT; then echo 'skip'; else echo 'enabled'; fi)"
    echo " Cargo runner: $CARGO_RUNNER_DESC"
    echo " Correlation id: $CORRELATION_ID"
    echo " Shard mode: $SHARD_KIND ($SHARD_NAME)"
    echo " Lib inline: enabled"
    echo " Integration targets: ${#SELECTED_UNIT_TARGETS[@]}"
    echo " E2E suites: ${#SELECTED_SUITES[@]}"
    if [[ -n "$RERUN_FROM" ]]; then
        echo " Rerun source: $RERUN_FROM"
    fi
    if [[ -n "$DIFF_FROM" ]]; then
        echo " Diff baseline: $DIFF_FROM"
    fi
    echo "═══════════════════════════════════════════════════════════════"
    echo ""

    if ! prepare_artifact_directory; then
        echo "[fatal] Artifact-directory preflight failed; refusing to reuse evidence." >&2
        exit 2
    fi

    if ! check_disk_headroom; then
        echo "[fatal] Preflight checks failed; refusing to start verification run." >&2
        exit 2
    fi

    if ! initialize_source_snapshot; then
        echo "[fatal] Source snapshot preflight failed; refusing to start verification run." >&2
        exit 2
    fi

    capture_env
    write_shard_manifest

    local overall_exit=0
    local source_snapshot_verified=true
    local runner_failed_phases=()

    record_runner_failure() {
        local phase="$1"
        local existing
        overall_exit=1
        for existing in "${runner_failed_phases[@]}"; do
            if [[ "$existing" == "$phase" ]]; then
                return 0
            fi
        done
        runner_failed_phases+=("$phase")
    }

    # Phase 1: Lint gates (fmt + clippy).
    if ! run_lint_gates; then
        record_runner_failure "lint"
    fi

    # Phase 2: Build all selected targets.
    if ! build_tests; then
        echo "[fatal] Build failed, aborting test run." >&2
        record_runner_failure "build"
        verify_source_snapshot_unchanged || true
        exit 1
    fi

    # Phase 3: Lib inline tests.
    if ! run_lib_tests; then
        record_runner_failure "lib"
    fi

    # Phase 4: Integration targets (unit + vcr test files).
    for target in "${SELECTED_UNIT_TARGETS[@]}"; do
        if ! run_unit_target "$target"; then
            record_runner_failure "unit:$target"
        fi
    done

    # Phase 5: E2E suites.
    for suite in "${SELECTED_SUITES[@]}"; do
        if [[ ! -f "tests/${suite}.rs" ]]; then
            echo "[suite] FAIL: $suite test file not found" >&2
            record_runner_failure "suite:$suite"
            continue
        fi
        if ! run_suite "$suite"; then
            record_runner_failure "suite:$suite"
        fi
    done

    if ! write_summary; then
        echo "[fatal] Unable to write the verification summary." >&2
        verify_source_snapshot_unchanged || true
        exit 1
    fi

    if ! generate_failure_diagnostics; then
        record_runner_failure "failure_diagnostics"
    fi

    if ! generate_replay_bundle; then
        record_runner_failure "replay_bundle"
    fi

    if ! generate_extension_profile_matrix; then
        record_runner_failure "extension_profile_matrix"
    fi

    if ! generate_soak_longevity_report; then
        record_runner_failure "soak_longevity_report"
    fi

    if ! generate_release_readiness_report; then
        record_runner_failure "release_readiness_report"
    fi

    if ! generate_triage_diff; then
        record_runner_failure "triage_diff"
    fi

    # The contract consumes a finalized aggregate outcome. Capture the source
    # once before contract generation, then recapture it again afterward to
    # preserve the producer's final TOCTOU guard.
    if ! verify_source_snapshot_unchanged; then
        source_snapshot_verified=false
        record_runner_failure "source_snapshot"
    fi

    if ! write_runner_outcome \
        "$overall_exit" \
        "$source_snapshot_verified" \
        "${runner_failed_phases[@]}"; then
        record_runner_failure "runner_outcome"
    fi

    if ! validate_evidence_contract; then
        record_runner_failure "evidence_contract"

        # A validator failure is itself part of the final runner outcome. Make
        # that state durable and regenerate the contract so a stale pass can
        # never survive beside a non-zero producer exit.
        if write_runner_outcome \
            "$overall_exit" \
            "$source_snapshot_verified" \
            "${runner_failed_phases[@]}"; then
            validate_evidence_contract || true
        fi
    fi

    if ! verify_source_snapshot_unchanged; then
        source_snapshot_verified=false
        record_runner_failure "source_snapshot_final"

        # The post-contract source recapture is the last pass/fail input. If it
        # fails, rewrite both artifacts so release consumers cannot observe a
        # passing contract from a run that ultimately returned non-zero.
        if write_runner_outcome \
            "$overall_exit" \
            "$source_snapshot_verified" \
            "${runner_failed_phases[@]}"; then
            validate_evidence_contract || true
        fi
    fi

    exit $overall_exit
}

main "$@"
