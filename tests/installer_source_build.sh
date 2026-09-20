#!/usr/bin/env bash
# Focused source-install regression tests. cargo and git below are shell
# functions: this harness never builds Rust, clones a repository, or uses I/O
# outside its temporary fixtures. Run with: bash tests/installer_source_build.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
INSTALLER="${PI_INSTALLER_TEST_SOURCE:-$ROOT/install.sh}"
set --
# shellcheck source=../install.sh
source "$INSTALLER"
# Sourcing declares the installer's exit trap; fixtures have their own lifetime.
trap - EXIT

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/pi-installer-source.XXXXXXXX")"
TEST_CASE=""
BUILD_MODE=success
GIT_MODE=success
OWNER=Dicklesworthstone
REPO=pi_agent_rust
VERSION=v0.0.0-test

fail() { printf 'FAIL: %s\nFixtures: %s\n' "$*" "$TEST_ROOT" >&2; exit 1; }
assert_arg() { grep -Fxq -- "$1" "$TEST_CASE/cargo.args" || fail "missing Cargo argument: $1"; }

# Deliberately model the precedence that caused GH-235. A test must not pass
# just because its fake Cargo always writes to the installer's expected path.
cargo() {
  printf '%s\n' "$@" > "$TEST_CASE/cargo.args"
  local target_dir="${CARGO_TARGET_DIR:-}"
  if [ -z "$target_dir" ] && [ -f .cargo/config.toml ]; then
    target_dir="$(sed -n 's/^target-dir = "\(.*\)"$/\1/p' .cargo/config.toml)"
  fi
  target_dir="${target_dir:-target}"
  local previous="" arg=""
  for arg in "$@"; do
    if [ "$previous" = --target-dir ]; then target_dir="$arg"; fi
    previous="$arg"
  done
  case "$BUILD_MODE" in
    fail) printf 'fixture compile failure\n' >&2; return 17 ;;
    missing) return 0 ;;
  esac
  mkdir -p "$target_dir/release"
  printf '#!/usr/bin/env bash\nprintf "fresh fixture binary\\n"\n' > "$target_dir/release/pi${EXE_EXT}"
  chmod +x "$target_dir/release/pi${EXE_EXT}"
  printf 'fixture build output must stay on stderr\n'
}

git() {
  printf '%s\n' "$@" > "$TEST_CASE/git.args"
  if [ "$GIT_MODE" = fail ]; then return 23; fi
  local destination="" arg=""
  for arg in "$@"; do destination="$arg"; done
  mkdir -p "$destination"
  printf '[package]\nname = "pi_agent_rust"\n' > "$destination/Cargo.toml"
}

new_case() {
  TEST_CASE="$TEST_ROOT/$1"
  SOURCE_DIR="$TEST_CASE/source with spaces"
  TMP="$TEST_CASE/tmp"
  OFFLINE=0
  EXE_EXT=""
  BUILD_MODE=success
  GIT_MODE=success
  unset CARGO_TARGET_DIR
  mkdir -p "$SOURCE_DIR/.cargo" "$TMP"
  printf '[package]\nname = "pi_agent_rust"\n' > "$SOURCE_DIR/Cargo.toml"
}

expect_success() {
  local expected="$1" output=""
  if ! output="$(build_from_source 2> "$TEST_CASE/stderr")"; then
    cat "$TEST_CASE/stderr" >&2
    fail "source build failed: $TEST_CASE"
  fi
  [ "$output" = "$expected" ] || fail "stdout was not the exact artifact path: $output"
  [ -x "$output" ] || fail "returned artifact is not executable"
  assert_arg --release
  assert_arg --locked
}

new_case default
expect_success "$SOURCE_DIR/target/release/pi"
if grep -Fxq -- --offline "$TEST_CASE/cargo.args"; then fail "online build forced offline"; fi

new_case environment
export CARGO_TARGET_DIR="$TEST_CASE/shared cache with spaces"
expect_success "$SOURCE_DIR/target/release/pi"
[ ! -e "$CARGO_TARGET_DIR" ] || fail "source install wrote into shared target directory"

new_case configuration
printf '[build]\ntarget-dir = "%s"\n' "$TEST_CASE/configured cache" > "$SOURCE_DIR/.cargo/config.toml"
expect_success "$SOURCE_DIR/target/release/pi"
[ ! -e "$TEST_CASE/configured cache" ] || fail "source install used configured target directory"
assert_arg --target-dir
assert_arg "$SOURCE_DIR/target"

new_case offline
OFFLINE=1
expect_success "$SOURCE_DIR/target/release/pi"
assert_arg --offline
[ ! -e "$TEST_CASE/git.args" ] || fail "local offline build attempted a clone"

new_case windows_suffix
EXE_EXT=.exe
expect_success "$SOURCE_DIR/target/release/pi.exe"

new_case failed_build_with_stale_binary
mkdir -p "$SOURCE_DIR/target/release"
printf 'old binary must survive\n' > "$SOURCE_DIR/target/release/pi"
chmod +x "$SOURCE_DIR/target/release/pi"
BUILD_MODE=fail
# The conditional intentionally disables errexit inside build_from_source.
if output="$(build_from_source 2> "$TEST_CASE/stderr")"; then fail "failed build returned stale binary"; fi
[ -z "$output" ] || fail "failed build returned an artifact path"
grep -Fq 'Source build failed' "$TEST_CASE/stderr" || fail "missing build failure diagnostic"
grep -Fxq 'old binary must survive' "$SOURCE_DIR/target/release/pi" || fail "stale binary was modified"

new_case missing_binary
BUILD_MODE=missing
if output="$(build_from_source 2> "$TEST_CASE/stderr")"; then fail "missing binary accepted"; fi
[ -z "$output" ] || fail "missing binary returned an artifact path"

new_case cloned_source
SOURCE_DIR=""
expect_success "$TMP/src/target/release/pi"
[ -f "$TEST_CASE/git.args" ] || fail "source acquisition was skipped"

new_case failed_clone
SOURCE_DIR=""
GIT_MODE=fail
if output="$(build_from_source 2> "$TEST_CASE/stderr")"; then fail "failed clone accepted"; fi
[ ! -e "$TEST_CASE/cargo.args" ] || fail "Cargo invoked after failed clone"
[ -z "$output" ] || fail "failed clone returned an artifact path"

new_case offline_without_source
SOURCE_DIR=""
OFFLINE=1
if output="$(build_from_source 2> "$TEST_CASE/stderr")"; then fail "offline clone accepted"; fi
[ ! -e "$TEST_CASE/git.args" ] || fail "offline clone attempted"
[ ! -e "$TEST_CASE/cargo.args" ] || fail "offline clone invoked Cargo"

printf 'PASS: 10 source-install scenarios (mock Cargo/Git; no Rust build)\nFixtures retained at %s\n' "$TEST_ROOT"
