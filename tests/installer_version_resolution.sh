#!/usr/bin/env bash
# GH-253: exercise the production resolver without installing or using a network.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
INSTALLER="${PI_INSTALLER_UNDER_TEST:-$ROOT/install.sh}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/pi-version-resolution.XXXXXXXX")"
cleanup() {
  rm -f "$WORK/requests" "$WORK/result" "$WORK/errors" "$WORK/resolver.sh"
  rmdir "$WORK"
}
trap cleanup EXIT

# Source only the actual resolver definitions, not installer initialization or
# its EXIT trap. A renamed/missing definition is an error, never a skipped test.
sed -n '/^normalize_version() {$/,/^detect_platform() {$/p' "$INSTALLER" \
  | sed '$d' > "$WORK/resolver.sh"
# shellcheck disable=SC1090
source "$WORK/resolver.sh"
declare -F normalize_version >/dev/null
declare -F resolve_version >/dev/null

info() { :; }
ok() { :; }
err() { printf '%s\n' "$*" >&2; }
# command -v must succeed even on hosts without a real curl. The network
# helpers below are the boundary fixtures; no external request can escape.
curl() { return 99; }
fetch_url_to_stdout() {
  printf 'metadata %s\n' "$1" >> "$WORK/requests"
  printf '%s' "$METADATA"
  return "$METADATA_RC"
}
fetch_effective_url() {
  printf 'redirect %s\n' "$1" >> "$WORK/requests"
  printf '%s' "$REDIRECT"
  return "$REDIRECT_RC"
}

reset_case() {
  OWNER=fixture-owner
  REPO=fixture-repo
  VERSION=''
  ARTIFACT_URL=''
  FROM_SOURCE=0
  OFFLINE=0
  METADATA=''
  METADATA_RC=0
  REDIRECT=''
  REDIRECT_RC=0
  : > "$WORK/requests"
}

assert_resolution() {
  local name="$1" expected="$2" expected_metadata="$3" expected_redirect="$4"
  local status=0
  (resolve_version; printf '%s' "$VERSION") > "$WORK/result" 2> "$WORK/errors" || status=$?
  local actual
  actual="$(cat "$WORK/result")"
  if [ "$status" -ne 0 ] || [ "$actual" != "$expected" ] \
    || [ "$(grep -c '^metadata ' "$WORK/requests" || true)" != "$expected_metadata" ] \
    || [ "$(grep -c '^redirect ' "$WORK/requests" || true)" != "$expected_redirect" ]; then
    printf 'FAIL %s: status=%s expected=%s actual=%s\n' "$name" "$status" "$expected" "$actual" >&2
    cat "$WORK/errors" "$WORK/requests" >&2
    exit 1
  fi
  printf 'PASS %s\n' "$name"
}

assert_refusal() {
  local name="$1" expected_error="$2"
  local status=0
  (resolve_version; printf '%s' "$VERSION") > "$WORK/result" 2> "$WORK/errors" || status=$?
  if [ "$status" -eq 0 ] || [ -s "$WORK/result" ] \
    || ! grep -Fq "$expected_error" "$WORK/errors"; then
    printf 'FAIL %s: expected refusal, got status=%s\n' "$name" "$status" >&2
    cat "$WORK/result" "$WORK/errors" >&2
    exit 1
  fi
  printf 'PASS %s\n' "$name"
}

reset_case
METADATA='{"tag_name":"v0.7.1","name":"A release","body":"Notes after the tag"}'
assert_resolution compact_metadata v0.7.1 1 0

reset_case
METADATA='{
  "tag_name": "v0.7.1",
  "name": "A release",
  "body": "Later notes"
}'
assert_resolution pretty_metadata v0.7.1 1 0

reset_case
METADATA=$'{"tag_name" \t:\r\n  "v0.7.1-rc.2", "body":"Notes"}'
assert_resolution whitespace_around_colon v0.7.1-rc.2 1 0

reset_case
METADATA='{"tag_name":"v0.7.1","body":"Use \"tag_name\": \"v9.9.9\" in examples"}'
assert_resolution escaped_body_key_is_not_metadata v0.7.1 1 0

reset_case
METADATA='{"body":"Use \"tag_name\": \"v9.9.9\" in examples","tag_name":"v0.7.1"}'
assert_resolution escaped_body_before_real_key v0.7.1 1 0

for metadata in \
  '{}' \
  '{"tag_name":null,"body":"Later notes"}' \
  '{"tag_name":"","body":"Later notes"}' \
  '{"body":"Only an escaped \"tag_name\": \"v9.9.9\" mention"}'; do
  reset_case
  METADATA="$metadata"
  REDIRECT='https://github.com/fixture-owner/fixture-repo/releases/tag/v0.7.1'
  assert_resolution absent_or_unusable_tag_uses_redirect v0.7.1 1 1
done

reset_case
METADATA='{"tag_name":"v9.9.9"}'
METADATA_RC=18
REDIRECT='https://github.com/fixture-owner/fixture-repo/releases/tag/v0.7.1'
assert_resolution failed_transfer_discards_partial_metadata v0.7.1 1 1

reset_case
METADATA_RC=22
REDIRECT_RC=22
assert_refusal failed_resolution 'Failed to resolve latest release tag'

reset_case
VERSION=0.7.1
assert_resolution explicit_version_skips_network v0.7.1 0 0

reset_case
ARTIFACT_URL='https://artifacts.invalid/pi'
assert_resolution custom_artifact_skips_network custom-artifact 0 0

reset_case
OFFLINE=1
assert_refusal offline_without_version 'Offline mode requires --version'
if [ -s "$WORK/requests" ]; then
  printf 'FAIL offline resolution attempted network access\n' >&2
  exit 1
fi

reset_case
OFFLINE=1
VERSION=v0.7.1
assert_resolution explicit_offline_version_skips_network v0.7.1 0 0

printf 'All 15 installer version-resolution cases passed.\n'
