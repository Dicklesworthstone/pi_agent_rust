#!/usr/bin/env bash
# Local checkouts must be installable without GitHub or a stale release cache.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
INSTALLER="${PI_INSTALLER_UNDER_TEST:-$ROOT/install.sh}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/pi-local-source.XXXXXXXX")"
cleanup() {
  rm -f "$WORK/functions.sh" "$WORK/requests" "$WORK/result" "$WORK/errors" "$WORK/pi"
  rmdir "$WORK"
}
trap cleanup EXIT

# Load production functions without running installer initialization or traps.
for name in normalize_version resolve_version check_network_preflight check_dependencies \
  should_skip_reinstall is_rust_pi_output; do
  sed -n "/^${name}() {$/,/^}$/p" "$INSTALLER" >> "$WORK/functions.sh"
done
# shellcheck disable=SC1090
source "$WORK/functions.sh"
for name in normalize_version resolve_version check_network_preflight check_dependencies \
  should_skip_reinstall is_rust_pi_output; do
  declare -F "$name" >/dev/null
done

info() { :; }
ok() { :; }
warn() { :; }
err() { printf '%s\n' "$*" >&2; }
fetch_url_to_stdout() { printf 'metadata\n' >> "$WORK/requests"; return 99; }
fetch_effective_url() { printf 'redirect\n' >> "$WORK/requests"; return 99; }
probe_url_head() { printf 'preflight\n' >> "$WORK/requests"; }
capture_version_line() { "$1" --version; }
# Dependency detection is the boundary fixture; no toolchain or network is used.
command() {
  if [ "${1:-}" = '-v' ]; then
    case "${2:-}" in
      cargo) [ "$CARGO_AVAILABLE" -eq 1 ]; return $? ;;
      git) [ "$GIT_AVAILABLE" -eq 1 ]; return $? ;;
      curl) return 0 ;;
    esac
  fi
  builtin command "$@"
}

cat > "$WORK/pi" <<'PI'
#!/usr/bin/env bash
printf '%s\n' 'pi 0.7.1 (fixture)'
PI
chmod +x "$WORK/pi"

reset_case() {
  OWNER=fixture-owner
  REPO=fixture-repo
  VERSION=''
  SOURCE_DIR="$WORK/source checkout"
  FROM_SOURCE=1
  OFFLINE=0
  ARTIFACT_URL=''
  FORCE_INSTALL=0
  INSTALL_BIN_PATH="$WORK/pi"
  PIAR_INSTALL_VERSION=''
  CARGO_AVAILABLE=1
  GIT_AVAILABLE=1
  : > "$WORK/requests"
}

FAILED=0
CASES=0
expect_status() {
  local expected="$1" name="$2" status=0
  shift 2
  ("$@") > "$WORK/result" 2> "$WORK/errors" || status=$?
  CASES=$((CASES + 1))
  if [ "$status" -ne "$expected" ]; then
    printf 'FAIL %s: expected status %s, got %s\n' "$name" "$expected" "$status" >&2
    cat "$WORK/result" "$WORK/errors" >&2
    FAILED=$((FAILED + 1))
  else
    printf 'PASS %s\n' "$name"
  fi
}

resolve_local_without_network() {
  resolve_version
  [ "$VERSION" = 'local-source' ] && [ ! -s "$WORK/requests" ]
}

reset_case
expect_status 0 local_source_does_not_resolve_a_release resolve_local_without_network

reset_case
OFFLINE=1
expect_status 0 offline_local_source_needs_no_version resolve_local_without_network

reset_case
OFFLINE=1
VERSION=0.7.1
explicit_local_version() {
  resolve_version
  [ "$VERSION" = v0.7.1 ] && [ ! -s "$WORK/requests" ]
}
expect_status 0 explicit_local_version_is_preserved explicit_local_version

reset_case
no_preflight_request() {
  check_network_preflight
  [ ! -s "$WORK/requests" ]
}
expect_status 0 local_source_skips_github_preflight no_preflight_request

reset_case
SOURCE_DIR=''
remote_preflight_request() {
  check_network_preflight
  grep -qx preflight "$WORK/requests"
}
expect_status 0 cloned_source_keeps_github_preflight remote_preflight_request

reset_case
GIT_AVAILABLE=0
expect_status 0 local_source_does_not_require_git check_dependencies

reset_case
SOURCE_DIR=''
GIT_AVAILABLE=0
expect_status 1 cloned_source_still_requires_git check_dependencies

reset_case
CARGO_AVAILABLE=0
expect_status 1 local_source_still_requires_cargo check_dependencies

reset_case
VERSION=local-source
PIAR_INSTALL_VERSION=local-source
expect_status 1 repeated_local_install_must_rebuild should_skip_reinstall

reset_case
VERSION=v0.7.1
PIAR_INSTALL_VERSION=v0.7.1
expect_status 1 explicit_local_tag_does_not_hide_checkout_changes should_skip_reinstall

reset_case
SOURCE_DIR=''
FROM_SOURCE=0
VERSION=v0.7.1
PIAR_INSTALL_VERSION=v0.7.1
expect_status 0 unchanged_release_can_skip_reinstall should_skip_reinstall
FORCE_INSTALL=1
expect_status 1 forced_release_install_still_rebuilds should_skip_reinstall

if [ "$FAILED" -ne 0 ]; then
  printf '%s of %s local-source installer cases failed.\n' "$FAILED" "$CASES" >&2
  exit 1
fi
printf 'All %s local-source installer cases passed.\n' "$CASES"
