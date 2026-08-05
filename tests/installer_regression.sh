#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="${ROOT}/install.sh"
UNINSTALLER="${ROOT}/uninstall.sh"
SKILL_SMOKE="${ROOT}/scripts/skill-smoke.sh"
WORK_ROOT="${TMPDIR:-/tmp}/pi-installer-regression-$(date -u +%Y%m%dT%H%M%SZ)-$$"

PASS_COUNT=0
FAIL_COUNT=0

mkdir -p "${WORK_ROOT}"

usage() {
  cat <<'USAGE'
Usage: tests/installer_regression.sh

Runs installer-focused regression checks for:
  - option parsing
  - release workflow install-command safety
  - checksum verification branches
  - sigstore/cosign verification branches
  - completion installation branches
USAGE
}

sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
    return 0
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
    return 0
  fi
  echo "missing sha256 tool (sha256sum or shasum)" >&2
  return 1
}

case_dir() {
  local name="$1"
  local dir="${WORK_ROOT}/${name}"
  mkdir -p "$dir/home" "$dir/state" "$dir/data" "$dir/config" "$dir/dest" "$dir/fixtures" "$dir/fakebin"
  printf '%s\n' "$dir"
}

write_existing_pi_stub() {
  local dir="$1"
  cat > "${dir}/fakebin/pi" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "--version" ]; then
  echo "pi 0.1.0 (existing-rust-stub)"
  exit 0
fi
echo "existing pi stub"
STUB
  chmod +x "${dir}/fakebin/pi"
}

write_existing_rpi_stub() {
  local dir="$1"
  cat > "${dir}/fakebin/rpi" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "--version" ]; then
  echo "rpi 1.2.3 (existing-stub)"
  exit 0
fi
echo "existing rpi stub"
STUB
  chmod +x "${dir}/fakebin/rpi"
}

write_cosign_stub() {
  local dir="$1"
  local mode="$2"
  cat > "${dir}/fakebin/cosign" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [ -n "\${COSIGN_LOG_PATH:-}" ]; then
  printf '%s\n' "\$*" >> "\${COSIGN_LOG_PATH}"
fi
if [ "${mode}" = "fail" ]; then
  echo "cosign fixture: forced failure" >&2
  exit 1
fi
exit 0
EOF
  chmod +x "${dir}/fakebin/cosign"
}

write_cp_fail_stub() {
  local dir="$1"
  cat > "${dir}/fakebin/cp" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
for arg in "$@"; do
  if [[ "$arg" == *"/skills/"* ]]; then
    echo "cp fixture: forced failure" >&2
    exit 1
  fi
done
/bin/cp "$@"
STUB
  chmod +x "${dir}/fakebin/cp"
}

write_uname_stub() {
  local dir="$1"
  local stub_os="$2"
  local stub_arch="$3"
  cat > "${dir}/fakebin/uname" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [ "\${1:-}" = "-s" ]; then
  echo "${stub_os}"
  exit 0
fi
if [ "\${1:-}" = "-m" ]; then
  echo "${stub_arch}"
  exit 0
fi
/usr/bin/uname "\$@"
EOF
  chmod +x "${dir}/fakebin/uname"
}

write_sysctl_stub() {
  local dir="$1"
  local arm64_capable="$2"
  local translated="${3:-0}"
  cat > "${dir}/fakebin/sysctl" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [ "\${1:-}" = "-in" ] || [ "\${1:-}" = "-n" ]; then
  key="\${2:-}"
  case "\$key" in
    hw.optional.arm64)
      echo "${arm64_capable}"
      exit 0
      ;;
    sysctl.proc_translated)
      echo "${translated}"
      exit 0
      ;;
  esac
fi
/usr/sbin/sysctl "\$@"
EOF
  chmod +x "${dir}/fakebin/sysctl"
}

write_timeout_unusable_stubs() {
  local dir="$1"
  cat > "${dir}/fakebin/timeout" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
exit 127
STUB
  chmod +x "${dir}/fakebin/timeout"

  cat > "${dir}/fakebin/gtimeout" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
exit 127
STUB
  chmod +x "${dir}/fakebin/gtimeout"
}

write_curl_artifact_stub() {
  local dir="$1"
  cat > "${dir}/fakebin/curl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

if [ -n "${CURL_LOG_PATH:-}" ]; then
  printf '%s\n' "$*" >> "${CURL_LOG_PATH}"
fi

output=""
is_head=0
args=("$@")
idx=0
while [ "$idx" -lt "${#args[@]}" ]; do
  arg="${args[$idx]}"
  case "$arg" in
    -I|-SI|-sI|-fsSLI)
      is_head=1
      ;;
    -o)
      idx=$((idx + 1))
      output="${args[$idx]}"
      ;;
  esac
  idx=$((idx + 1))
done

if [ "$is_head" -eq 1 ]; then
  exit 0
fi

url="${args[${#args[@]}-1]}"
if [ -n "$output" ] && [ -n "${STUB_ARTIFACT_SOURCE:-}" ]; then
  cp "${STUB_ARTIFACT_SOURCE}" "$output"
  exit 0
fi

if [ -n "$output" ] && [[ "$url" == file://* ]]; then
  cp "${url#file://}" "$output"
  exit 0
fi

if [ -n "$output" ]; then
  : > "$output"
  exit 0
fi

exit 0
STUB
  chmod +x "${dir}/fakebin/curl"
}

write_artifact_binary() {
  local path="$1"
  local mode="$2"
  cat > "$path" <<EOF
#!/usr/bin/env bash
set -euo pipefail
MODE="${mode}"

if [ "\${1:-}" = "--version" ]; then
  echo "pi 9.9.9 (fixture)"
  exit 0
fi

if [ "\${1:-}" = "--help" ]; then
  case "\${MODE}" in
    help_lists_completions)
      cat <<'HELP'
Usage: pi [OPTIONS] [ARGS]... [COMMAND]

Commands:
  completions  Generate shell completions
  help         Print this message
HELP
      exit 0
      ;;
    help_inconclusive_probe_ok)
      cat <<'HELP'
Usage: pi [OPTIONS] [ARGS]... [COMMAND]
This build omits the command table from --help output.
HELP
      exit 0
      ;;
    help_conclusive_no_completion)
      cat <<'HELP'
Usage: pi [OPTIONS] [ARGS]... [COMMAND]

Commands:
  help  Print this message
HELP
      exit 0
      ;;
    *)
      exit 1
      ;;
  esac
fi

if [ "\${1:-}" = "completions" ]; then
  if [ "\${2:-}" = "--help" ]; then
    if [ "\${MODE}" = "unsupported" ]; then
      exit 1
    fi
    if [ "\${MODE}" = "completion_probe_hang" ]; then
      sleep "\${STUB_COMPLETION_SLEEP_SECS:-30}"
      exit 1
    fi
    exit 0
  fi

  case "\${MODE}" in
    completion_fail)
      exit 1
      ;;
    completion_empty)
      exit 0
      ;;
    completion_ok)
      case "\${2:-}" in
        bash)
          echo "# bash completion for pi fixture"
          exit 0
          ;;
        zsh)
          echo "#compdef pi"
          exit 0
          ;;
        fish)
          echo "complete -c pi"
          exit 0
          ;;
        *)
          exit 1
          ;;
      esac
      ;;
    help_lists_completions|help_inconclusive_probe_ok)
      case "\${2:-}" in
        bash)
          echo "# bash completion for pi fixture"
          exit 0
          ;;
        zsh)
          echo "#compdef pi"
          exit 0
          ;;
        fish)
          echo "complete -c pi"
          exit 0
          ;;
        *)
          exit 1
          ;;
      esac
      ;;
    help_conclusive_no_completion)
      sleep "\${STUB_COMPLETION_SLEEP_SECS:-30}"
      exit 1
      ;;
    completion_hang)
      sleep "\${STUB_COMPLETION_SLEEP_SECS:-30}"
      echo "# delayed completion output"
      exit 0
      ;;
    completion_probe_hang)
      sleep "\${STUB_COMPLETION_SLEEP_SECS:-30}"
      exit 1
      ;;
    *)
      exit 1
      ;;
  esac
fi

if [ "\${1:-}" = "completion" ]; then
  if [ "\${2:-}" = "--help" ]; then
    if [ "\${MODE}" = "completion_probe_hang" ]; then
      sleep "\${STUB_COMPLETION_SLEEP_SECS:-30}"
    fi
    if [ "\${MODE}" = "help_conclusive_no_completion" ]; then
      sleep "\${STUB_COMPLETION_SLEEP_SECS:-30}"
    fi
    exit 1
  fi
  exit 1
fi

exit 1
EOF
  chmod +x "$path"
}

run_installer() {
  local dir="$1"
  shift
  local out="${dir}/output.log"
  local rc_file="${dir}/exit_code"
  local path_value="${dir}/fakebin:/usr/bin:/bin"
  local run_cwd="${PI_INSTALLER_TEST_CWD:-$PWD}"

  (
    set +e
    cd "$run_cwd" || exit 1
    HOME="${dir}/home" \
    XDG_STATE_HOME="${dir}/state" \
    XDG_DATA_HOME="${dir}/data" \
    XDG_CONFIG_HOME="${dir}/config" \
    PATH="${path_value}" \
    SHELL="/bin/bash" \
    bash "${INSTALLER}" "$@" >"${out}" 2>&1
    echo "$?" > "${rc_file}"
  )
}

run_uninstaller() {
  local dir="$1"
  shift
  local out="${dir}/output.log"
  local rc_file="${dir}/exit_code"
  local path_value="${dir}/fakebin:/usr/bin:/bin"

  (
    set +e
    HOME="${dir}/home" \
    XDG_STATE_HOME="${dir}/state" \
    XDG_DATA_HOME="${dir}/data" \
    XDG_CONFIG_HOME="${dir}/config" \
    PATH="${path_value}" \
    SHELL="/bin/bash" \
    bash "${UNINSTALLER}" "$@" >"${out}" 2>&1
    echo "$?" > "${rc_file}"
  )
}

exit_code_of() {
  local dir="$1"
  cat "${dir}/exit_code"
}

assert_exit_code() {
  local dir="$1"
  local expected="$2"
  local actual
  actual="$(exit_code_of "$dir")"
  if [ "$actual" != "$expected" ]; then
    echo "expected exit ${expected}, got ${actual}" >&2
    echo "--- output (${dir}) ---" >&2
    cat "${dir}/output.log" >&2
    return 1
  fi
}

assert_output_contains() {
  local dir="$1"
  local needle="$2"
  if ! grep -Fq -- "$needle" "${dir}/output.log"; then
    echo "missing output text: ${needle}" >&2
    echo "--- output (${dir}) ---" >&2
    cat "${dir}/output.log" >&2
    return 1
  fi
}

assert_file_contains() {
  local file="$1"
  local needle="$2"
  if ! grep -Fq -- "$needle" "$file"; then
    echo "missing file text in ${file}: ${needle}" >&2
    echo "--- file (${file}) ---" >&2
    cat "$file" >&2
    return 1
  fi
}

run_test() {
  local name="$1"
  if "$name"; then
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "[PASS] ${name}"
  else
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo "[FAIL] ${name}"
  fi
}

test_help_lists_installer_flags() {
  local dir
  dir="$(case_dir "help-flags")"
  write_existing_pi_stub "$dir"
  run_installer "$dir" --help
  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "--artifact-url URL"
  assert_output_contains "$dir" "--checksum HEX"
  assert_output_contains "$dir" "--sigstore-bundle-url URL"
  assert_output_contains "$dir" "--completions SHELL"
  assert_output_contains "$dir" "--no-agent-skills"
}

test_release_workflows_do_not_use_no_verify() {
  local matches
  matches="$(grep -RIn -- '--no-verify' "${ROOT}/.github/workflows" 2>/dev/null || true)"
  if [ -n "$matches" ]; then
    echo "release/workflow install commands must not use --no-verify" >&2
    echo "$matches" >&2
    return 1
  fi
}

test_skill_smoke_script_passes() {
  local dir
  dir="$(case_dir "skill-smoke-script")"

  if ! (
    cd "$ROOT"
    bash "$SKILL_SMOKE" > "${dir}/output.log" 2>&1
  ); then
    echo "skill smoke script failed" >&2
    cat "${dir}/output.log" >&2
    return 1
  fi
}

test_invalid_completions_value_fails() {
  local dir
  dir="$(case_dir "invalid-completions")"
  write_existing_pi_stub "$dir"
  run_installer "$dir" --completions nope --no-gum
  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "Invalid --completions value"
}

test_unknown_option_fails() {
  local dir
  dir="$(case_dir "unknown-option")"
  write_existing_pi_stub "$dir"
  run_installer "$dir" --totally-unknown-flag
  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "Unknown option"
}

test_missing_option_value_fails() {
  local dir
  dir="$(case_dir "missing-option-value")"
  write_existing_pi_stub "$dir"
  run_installer "$dir" --version
  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "Option --version requires a value"
}

test_missing_option_value_when_next_arg_is_flag_fails() {
  local dir
  dir="$(case_dir "missing-option-value-next-flag")"
  write_existing_pi_stub "$dir"
  run_installer "$dir" --version --no-gum
  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "Option --version requires a value"
}

test_custom_artifact_download_failure_does_not_source_fallback_without_version() {
  local dir missing_artifact
  dir="$(case_dir "custom-artifact-no-version-fallback")"
  write_existing_pi_stub "$dir"
  missing_artifact="${dir}/fixtures/missing-pi"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --dest "${dir}/dest" \
    --artifact-url "file://${missing_artifact}" \
    --no-completions

  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "Custom artifact download failed; cannot fall back to source without a release tag"
  assert_output_contains "$dir" "Pass --version vX.Y.Z with --artifact-url, or use --from-source directly"
}

test_offline_tarball_mode_installs_local_artifact() {
  local dir artifact offline_dir tarball checksum installed
  dir="$(case_dir "offline-tarball-mode")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"

  offline_dir="${dir}/fixtures/offline-root"
  mkdir -p "$offline_dir"
  cp "$artifact" "${offline_dir}/pi"
  tar -czf "${dir}/fixtures/pi-offline.tar.gz" -C "$offline_dir" pi

  tarball="${dir}/fixtures/pi-offline.tar.gz"
  checksum="$(sha256_file "$tarball")"

  run_installer "$dir" \
    --yes --no-gum \
    --offline "$tarball" \
    --dest "${dir}/dest" \
    --checksum "$checksum" \
    --no-completions \
    --no-agent-skills

  installed="${dir}/dest/pi"

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Offline artifact mode enabled"
  [ -x "$installed" ] || { echo "expected installed binary at ${installed}" >&2; return 1; }
}

test_offline_mode_blocks_network_artifact_urls() {
  local dir
  dir="$(case_dir "offline-blocks-network")"
  write_existing_pi_stub "$dir"

  run_installer "$dir" \
    --yes --no-gum \
    --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "https://example.invalid/pi-fixture" \
    --checksum "0000000000000000000000000000000000000000000000000000000000000000" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "Offline mode requires a local --artifact-url path"
}

test_offline_relative_tarball_path_is_accepted() {
  local dir artifact tarball checksum installed
  dir="$(case_dir "offline-relative-tarball")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  cp "$artifact" "${dir}/fixtures/pi"
  tar -czf "${dir}/fixtures/relative-offline.tar.gz" -C "${dir}/fixtures" pi

  tarball="fixtures/relative-offline.tar.gz"
  checksum="$(sha256_file "${dir}/fixtures/relative-offline.tar.gz")"

  (
    cd "$dir"
    run_installer "$dir" \
      --yes --no-gum \
      --offline "$tarball" \
      --dest "${dir}/dest" \
      --checksum "$checksum" \
      --no-completions \
      --no-agent-skills
  )

  installed="${dir}/dest/pi"
  assert_exit_code "$dir" 0
  [ -x "$installed" ] || { echo "expected installed binary at ${installed}" >&2; return 1; }
}

test_proxy_args_are_applied_to_curl_downloads() {
  local dir artifact checksum curl_log
  dir="$(case_dir "proxy-args-curl")"
  write_existing_pi_stub "$dir"
  write_curl_artifact_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  checksum="$(sha256_file "$artifact")"
  curl_log="${dir}/curl.log"

  HTTPS_PROXY="https://proxy.example.test:8443" \
  STUB_ARTIFACT_SOURCE="$artifact" \
  CURL_LOG_PATH="$curl_log" \
  run_installer "$dir" \
    --yes --no-gum \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "https://example.invalid/pi-fixture" \
    --checksum "$checksum" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Using HTTPS proxy from environment"
  if ! grep -Fq -- "--proxy https://proxy.example.test:8443" "$curl_log"; then
    echo "expected --proxy arg in curl invocation" >&2
    cat "$curl_log" >&2
    return 1
  fi
}

test_linux_target_uses_supported_linux_artifact_naming() {
  local dir artifact checksum curl_log
  dir="$(case_dir "linux-target-gnu")"
  write_existing_pi_stub "$dir"
  write_uname_stub "$dir" "Linux" "x86_64"
  write_curl_artifact_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  checksum="$(sha256_file "$artifact")"
  curl_log="${dir}/curl.log"

  STUB_ARTIFACT_SOURCE="$artifact" \
  CURL_LOG_PATH="$curl_log" \
  run_installer "$dir" \
    --yes --no-gum \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --checksum "$checksum" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
  if ! grep -Eq "pi_linux_amd64|x86_64-unknown-linux-gnu" "$curl_log"; then
    echo "expected linux-amd64 or GNU artifact URL candidate" >&2
    cat "$curl_log" >&2
    return 1
  fi
}

test_rosetta_prefers_arm64_artifact_naming() {
  local dir artifact checksum curl_log
  dir="$(case_dir "rosetta-prefers-arm64")"
  write_existing_pi_stub "$dir"
  write_uname_stub "$dir" "Darwin" "x86_64"
  write_sysctl_stub "$dir" "1" "1"
  write_curl_artifact_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  checksum="$(sha256_file "$artifact")"
  curl_log="${dir}/curl.log"

  STUB_ARTIFACT_SOURCE="$artifact" \
  CURL_LOG_PATH="$curl_log" \
  run_installer "$dir" \
    --yes --no-gum \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --checksum "$checksum" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Rosetta shell detected on Apple Silicon; preferring native arm64 binary"
  if ! grep -Eq "pi_darwin_arm64|aarch64-apple-darwin|pi-darwin-arm64" "$curl_log"; then
    echo "expected darwin arm64 artifact URL candidate under Rosetta" >&2
    cat "$curl_log" >&2
    return 1
  fi
}

test_wsl_detection_warning_is_emitted() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "wsl-detection-warning")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  PI_INSTALLER_TEST_FORCE_WSL=1 \
  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "WSL detected"
}

test_installer_creates_rpi_alias_when_available() {
  local dir artifact artifact_url checksum install_bin compat_alias
  dir="$(case_dir "installer-creates-rpi-alias")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"
  install_bin="${dir}/dest/pi"
  compat_alias="${dir}/dest/rpi"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Alias:     installed (rpi -> ${install_bin})"
  [ -x "$install_bin" ] || { echo "expected installed binary at ${install_bin}" >&2; return 1; }
  [ -x "$compat_alias" ] || { echo "expected compatibility alias at ${compat_alias}" >&2; return 1; }
  grep -Fq "pi_agent_rust installer managed alias" "$compat_alias" || {
    echo "expected managed alias marker in ${compat_alias}" >&2
    return 1
  }
  "${compat_alias}" --version | grep -Fq "pi 9.9.9 (fixture)" || {
    echo "expected compatibility alias to execute installed pi binary" >&2
    return 1
  }
}

test_installer_skips_rpi_alias_when_existing_command_present() {
  local dir artifact artifact_url checksum compat_alias
  dir="$(case_dir "installer-skips-rpi-alias-conflict")"
  write_existing_pi_stub "$dir"
  write_existing_rpi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"
  compat_alias="${dir}/dest/rpi"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Existing rpi command detected at ${dir}/fakebin/rpi; skipping managed alias"
  assert_output_contains "$dir" "Alias:     skipped (existing rpi command at ${dir}/fakebin/rpi)"
  [ ! -e "$compat_alias" ] || {
    echo "installer should not create rpi alias when another rpi command already exists" >&2
    return 1
  }
}

test_legacy_agent_settings_cleanup_is_safe_and_idempotent() {
  local dir artifact artifact_url checksum state_file install_bin claude_settings gemini_settings
  dir="$(case_dir "legacy-agent-settings-cleanup")"
  write_existing_pi_stub "$dir"

  install_bin="${dir}/dest/pi"
  claude_settings="${dir}/home/.claude/settings.json"
  gemini_settings="${dir}/home/.gemini/settings.json"
  mkdir -p "$(dirname "$claude_settings")" "$(dirname "$gemini_settings")"

  cat > "$claude_settings" <<JSON
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {"type":"command","command":"${install_bin}"},
          {"type":"command","command":"${install_bin}","label":"keep-me"},
          {"type":"command","command":"/usr/bin/pipx"}
        ]
      }
    ]
  }
}
JSON

  cat > "$gemini_settings" <<JSON
{
  "hooks": {
    "BeforeTool": [
      {
        "matcher": "run_shell_command",
        "hooks": [
          {"name":"pi-agent-rust","type":"command","command":"${install_bin}","timeout":5000},
          {"name":"pi-agent-rust","type":"command","command":"${install_bin}","timeout":7000},
          {"name":"legacy","type":"command","command":"${install_bin}","timeout":5000}
        ]
      }
    ]
  }
}
JSON

  state_file="${dir}/state/pi-agent-rust/install-state.env"
  mkdir -p "$(dirname "$state_file")"
  cat > "$state_file" <<STATE
PIAR_INSTALL_BIN='${install_bin}'
STATE

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
  if [ "$(grep -Ec "\"command\"[[:space:]]*:[[:space:]]*\"${install_bin}\"" "$claude_settings")" -ne 1 ]; then
    echo "expected exactly one Claude command entry for ${install_bin} after cleanup" >&2
    cat "$claude_settings" >&2
    return 1
  fi
  if ! grep -Eq "\"label\"[[:space:]]*:[[:space:]]*\"keep-me\"" "$claude_settings"; then
    echo "expected custom Claude entry to remain after cleanup" >&2
    cat "$claude_settings" >&2
    return 1
  fi
  if ! grep -Eq "\"command\"[[:space:]]*:[[:space:]]*\"/usr/bin/pipx\"" "$claude_settings"; then
    echo "expected non-installer Claude entry to remain after cleanup" >&2
    cat "$claude_settings" >&2
    return 1
  fi

  if [ "$(grep -Ec "\"name\"[[:space:]]*:[[:space:]]*\"pi-agent-rust\"" "$gemini_settings")" -ne 1 ]; then
    echo "expected only the non-default pi-agent-rust Gemini entry to remain after cleanup" >&2
    cat "$gemini_settings" >&2
    return 1
  fi
  if ! grep -Eq "\"timeout\"[[:space:]]*:[[:space:]]*7000" "$gemini_settings"; then
    echo "expected custom-timeout Gemini entry to remain after cleanup" >&2
    cat "$gemini_settings" >&2
    return 1
  fi
  if ! grep -Eq "\"name\"[[:space:]]*:[[:space:]]*\"legacy\"" "$gemini_settings"; then
    echo "expected non-installer Gemini entry to remain after cleanup" >&2
    cat "$gemini_settings" >&2
    return 1
  fi

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
}

test_legacy_cleanup_skips_unexpected_settings_paths() {
  local dir artifact artifact_url checksum state_file install_bin unexpected_settings
  dir="$(case_dir "legacy-agent-settings-unexpected-path")"
  write_existing_pi_stub "$dir"

  install_bin="${dir}/dest/pi"
  unexpected_settings="${dir}/home/custom/settings.json"
  mkdir -p "$(dirname "$unexpected_settings")"
  cat > "$unexpected_settings" <<JSON
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {"type":"command","command":"${install_bin}"}
        ]
      }
    ]
  }
}
JSON

  state_file="${dir}/state/pi-agent-rust/install-state.env"
  mkdir -p "$(dirname "$state_file")"
  cat > "$state_file" <<STATE
PIAR_INSTALL_BIN='${install_bin}'
PIAR_CLAUDE_HOOK_SETTINGS='${unexpected_settings}'
STATE

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions \
    --no-agent-skills

  assert_exit_code "$dir" 0
  if ! grep -Eq "\"command\"[[:space:]]*:[[:space:]]*\"${install_bin}\"" "$unexpected_settings"; then
    echo "unexpected settings path should remain untouched by cleanup" >&2
    cat "$unexpected_settings" >&2
    return 1
  fi
}

test_agent_skills_install_by_default() {
  local dir artifact artifact_url checksum claude_skill codex_skill
  local claude_commands codex_commands claude_debugging codex_debugging
  dir="$(case_dir "agent-skills-default")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions

  claude_skill="${dir}/home/.claude/skills/pi-agent-rust/SKILL.md"
  codex_skill="${dir}/home/.codex/skills/pi-agent-rust/SKILL.md"
  claude_commands="${dir}/home/.claude/skills/pi-agent-rust/references/COMMANDS.md"
  codex_commands="${dir}/home/.codex/skills/pi-agent-rust/references/COMMANDS.md"
  claude_debugging="${dir}/home/.claude/skills/pi-agent-rust/references/DEBUGGING-PLAYBOOKS.md"
  codex_debugging="${dir}/home/.codex/skills/pi-agent-rust/references/DEBUGGING-PLAYBOOKS.md"

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Skills:    installed (claude,codex)"
  [ -f "$claude_skill" ] || { echo "missing Claude skill: $claude_skill" >&2; return 1; }
  [ -f "$codex_skill" ] || { echo "missing Codex skill: $codex_skill" >&2; return 1; }
  [ -f "$claude_commands" ] || { echo "missing Claude commands reference: $claude_commands" >&2; return 1; }
  [ -f "$codex_commands" ] || { echo "missing Codex commands reference: $codex_commands" >&2; return 1; }
  [ -f "$claude_debugging" ] || { echo "missing Claude debugging reference: $claude_debugging" >&2; return 1; }
  [ -f "$codex_debugging" ] || { echo "missing Codex debugging reference: $codex_debugging" >&2; return 1; }
  grep -Fq "pi_agent_rust installer managed skill" "$claude_skill" || {
    echo "missing managed marker in Claude skill" >&2
    return 1
  }
  grep -Fq "pi_agent_rust installer managed skill" "$codex_skill" || {
    echo "missing managed marker in Codex skill" >&2
    return 1
  }
  grep -Fq "## High-Value Commands" "$claude_skill" || {
    echo "installed skill should include high-value command section" >&2
    return 1
  }
  grep -Fq "## 8) Status and Safety Tracing" "$claude_commands" || {
    echo "installed Claude command references should include command recipes" >&2
    return 1
  }
  grep -Fq "## Playbook 4: Installer / Uninstaller / Skill Installation Failures" "$claude_debugging" || {
    echo "installed Claude debugging references should include playbooks" >&2
    return 1
  }
}

test_agent_skill_install_ignores_shadow_pwd_skill() {
  local dir artifact artifact_url checksum claude_skill codex_skill shadow_skill
  local claude_commands
  dir="$(case_dir "agent-skills-ignore-shadow-pwd")"
  write_existing_pi_stub "$dir"

  mkdir -p "${dir}/shadow/.claude/skills/pi-agent-rust"
  shadow_skill="${dir}/shadow/.claude/skills/pi-agent-rust/SKILL.md"
  cat > "$shadow_skill" <<'SKILL'
# SHADOW SKILL FROM PWD
SKILL

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  PI_INSTALLER_TEST_CWD="${dir}/shadow" run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions

  claude_skill="${dir}/home/.claude/skills/pi-agent-rust/SKILL.md"
  codex_skill="${dir}/home/.codex/skills/pi-agent-rust/SKILL.md"
  claude_commands="${dir}/home/.claude/skills/pi-agent-rust/references/COMMANDS.md"

  assert_exit_code "$dir" 0
  [ -f "$claude_skill" ] || { echo "missing Claude skill after shadow cwd install" >&2; return 1; }
  [ -f "$codex_skill" ] || { echo "missing Codex skill after shadow cwd install" >&2; return 1; }
  [ -f "$claude_commands" ] || { echo "missing Claude reference docs after shadow cwd install" >&2; return 1; }
  if grep -Fq "SHADOW SKILL FROM PWD" "$claude_skill"; then
    echo "installer should ignore shadow skill content from PWD" >&2
    return 1
  fi
}

test_no_agent_skills_opt_out() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "agent-skills-opt-out")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-agent-skills \
    --no-completions

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Skills:    skipped (--no-agent-skills)"
  if [ -e "${dir}/home/.claude/skills/pi-agent-rust/SKILL.md" ]; then
    echo "Claude skill should not be installed when --no-agent-skills is used" >&2
    return 1
  fi
  if [ -e "${dir}/home/.codex/skills/pi-agent-rust/SKILL.md" ]; then
    echo "Codex skill should not be installed when --no-agent-skills is used" >&2
    return 1
  fi
}

test_existing_custom_skill_dirs_are_not_overwritten() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "agent-skills-custom-preserve")"
  write_existing_pi_stub "$dir"

  mkdir -p "${dir}/home/.claude/skills/pi-agent-rust"
  mkdir -p "${dir}/home/.codex/skills/pi-agent-rust"
  printf 'custom\n' > "${dir}/home/.claude/skills/pi-agent-rust/NOT_A_SKILL.txt"
  printf 'custom\n' > "${dir}/home/.codex/skills/pi-agent-rust/NOT_A_SKILL.txt"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Skills:    skipped (existing custom skill)"
  [ -f "${dir}/home/.claude/skills/pi-agent-rust/NOT_A_SKILL.txt" ] || {
    echo "Claude custom skill dir should be preserved" >&2
    return 1
  }
  [ -f "${dir}/home/.codex/skills/pi-agent-rust/NOT_A_SKILL.txt" ] || {
    echo "Codex custom skill dir should be preserved" >&2
    return 1
  }
}

test_skill_copy_failure_preserves_existing_managed_skills() {
  local dir artifact artifact_url checksum claude_skill codex_skill
  dir="$(case_dir "agent-skills-copy-fail-preserve-existing")"
  write_existing_pi_stub "$dir"
  write_cp_fail_stub "$dir"

  claude_skill="${dir}/home/.claude/skills/pi-agent-rust/SKILL.md"
  codex_skill="${dir}/home/.codex/skills/pi-agent-rust/SKILL.md"
  mkdir -p "$(dirname "$claude_skill")" "$(dirname "$codex_skill")"
  cat > "$claude_skill" <<'SKILL'
<!-- pi_agent_rust installer managed skill -->
# OLD CLAUDE SKILL
SKILL
  cat > "$codex_skill" <<'SKILL'
<!-- pi_agent_rust installer managed skill -->
# OLD CODEX SKILL
SKILL

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Skills:    failed (unable to write skill files)"
  grep -Fq "OLD CLAUDE SKILL" "$claude_skill" || {
    echo "existing managed Claude skill should be preserved when copy fails" >&2
    return 1
  }
  grep -Fq "OLD CODEX SKILL" "$codex_skill" || {
    echo "existing managed Codex skill should be preserved when copy fails" >&2
    return 1
  }
}

test_skill_custom_plus_copy_failure_reports_partial() {
  local dir artifact artifact_url checksum codex_custom
  dir="$(case_dir "agent-skills-custom-plus-copy-fail-partial")"
  write_existing_pi_stub "$dir"
  write_cp_fail_stub "$dir"

  codex_custom="${dir}/home/.codex/skills/pi-agent-rust/SKILL.md"
  mkdir -p "$(dirname "$codex_custom")"
  cat > "$codex_custom" <<'SKILL'
# Custom Codex skill without installer marker
SKILL

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Skills:    partial (custom skill kept; other install failed)"
  [ -f "$codex_custom" ] || {
    echo "custom Codex skill should be preserved" >&2
    return 1
  }
  if [ -f "${dir}/home/.claude/skills/pi-agent-rust/SKILL.md" ]; then
    echo "Claude skill should not be created when copy fails" >&2
    return 1
  fi
}

test_uninstall_removes_only_installer_managed_skills() {
  local dir managed_skill custom_skill
  dir="$(case_dir "uninstall-managed-skills-only")"

  managed_skill="${dir}/home/.claude/skills/pi-agent-rust/SKILL.md"
  custom_skill="${dir}/home/.codex/skills/pi-agent-rust/SKILL.md"
  mkdir -p "$(dirname "$managed_skill")" "$(dirname "$custom_skill")"

  cat > "$managed_skill" <<'SKILL'
<!-- pi_agent_rust installer managed skill -->
# Managed skill
SKILL
  cat > "$custom_skill" <<'SKILL'
# Custom local skill (no installer marker)
SKILL

  run_uninstaller "$dir" --yes --no-gum

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Removed installer-managed skill: ${dir}/home/.claude/skills/pi-agent-rust"
  assert_output_contains "$dir" "Skipping non-managed skill directory: ${dir}/home/.codex/skills/pi-agent-rust"
  if [ -e "${dir}/home/.claude/skills/pi-agent-rust" ]; then
    echo "installer-managed Claude skill directory should be removed" >&2
    return 1
  fi
  if [ ! -f "${dir}/home/.codex/skills/pi-agent-rust/SKILL.md" ]; then
    echo "custom Codex skill directory should be preserved" >&2
    return 1
  fi
}

test_uninstall_removes_recorded_rpi_alias() {
  local dir state_dir state_file alias_path
  dir="$(case_dir "uninstall-removes-rpi-alias")"
  state_dir="${dir}/state/pi-agent-rust"
  state_file="${state_dir}/install-state.env"
  alias_path="${dir}/home/.local/bin/rpi"
  mkdir -p "${state_dir}" "$(dirname "$alias_path")"

  cat > "$alias_path" <<'ALIAS'
#!/usr/bin/env bash
# pi_agent_rust installer managed alias
set -euo pipefail
exec /tmp/pi "$@"
ALIAS
  chmod +x "$alias_path"

  cat > "$state_file" <<EOF
PIAR_COMPAT_ALIAS_PATH=${alias_path}
PIAR_COMPAT_ALIAS_STATUS='installed (rpi -> /tmp/pi)'
EOF

  run_uninstaller "$dir" --yes --no-gum

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Removed compatibility alias: ${alias_path}"
  if [ -e "$alias_path" ]; then
    echo "expected recorded compatibility alias to be removed" >&2
    return 1
  fi
}

test_uninstall_cleans_legacy_agent_settings_hooks() {
  local dir state_file install_bin claude_settings gemini_settings
  dir="$(case_dir "uninstall-legacy-agent-settings-cleanup")"

  install_bin="${dir}/dest/pi"
  claude_settings="${dir}/home/.claude/settings.json"
  gemini_settings="${dir}/home/.gemini/settings.json"
  mkdir -p "$(dirname "$claude_settings")" "$(dirname "$gemini_settings")"

  cat > "$claude_settings" <<JSON
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {"type":"command","command":"${install_bin}"},
          {"type":"command","command":"${install_bin}","label":"keep-me"}
        ]
      }
    ]
  }
}
JSON

  cat > "$gemini_settings" <<JSON
{
  "hooks": {
    "BeforeTool": [
      {
        "matcher": "run_shell_command",
        "hooks": [
          {"name":"pi-agent-rust","type":"command","command":"${install_bin}","timeout":5000},
          {"name":"pi-agent-rust","type":"command","command":"${install_bin}","timeout":7000}
        ]
      }
    ]
  }
}
JSON

  state_file="${dir}/state/pi-agent-rust/install-state.env"
  mkdir -p "$(dirname "$state_file")"
  cat > "$state_file" <<STATE
PIAR_INSTALL_BIN='${install_bin}'
STATE

  run_uninstaller "$dir" --yes --no-gum

  assert_exit_code "$dir" 0
  if [ "$(grep -Ec "\"command\"[[:space:]]*:[[:space:]]*\"${install_bin}\"" "$claude_settings")" -ne 1 ]; then
    echo "expected exactly one Claude command entry for ${install_bin} after uninstall cleanup" >&2
    cat "$claude_settings" >&2
    return 1
  fi
  if ! grep -Eq "\"label\"[[:space:]]*:[[:space:]]*\"keep-me\"" "$claude_settings"; then
    echo "expected custom Claude hook to remain after uninstall cleanup" >&2
    cat "$claude_settings" >&2
    return 1
  fi
  if [ "$(grep -Ec "\"name\"[[:space:]]*:[[:space:]]*\"pi-agent-rust\"" "$gemini_settings")" -ne 1 ]; then
    echo "expected only custom-timeout pi-agent-rust Gemini hook to remain after uninstall cleanup" >&2
    cat "$gemini_settings" >&2
    return 1
  fi
  if ! grep -Eq "\"timeout\"[[:space:]]*:[[:space:]]*7000" "$gemini_settings"; then
    echo "expected custom Gemini hook timeout to remain after uninstall cleanup" >&2
    cat "$gemini_settings" >&2
    return 1
  fi

  run_uninstaller "$dir" --yes --no-gum

  assert_exit_code "$dir" 0
  if [ "$(grep -Ec "\"command\"[[:space:]]*:[[:space:]]*\"${install_bin}\"" "$claude_settings")" -ne 1 ]; then
    echo "expected exactly one Claude command entry for ${install_bin} after second uninstall cleanup" >&2
    cat "$claude_settings" >&2
    return 1
  fi
  if ! grep -Eq "\"label\"[[:space:]]*:[[:space:]]*\"keep-me\"" "$claude_settings"; then
    echo "expected custom Claude hook to remain after second uninstall cleanup" >&2
    cat "$claude_settings" >&2
    return 1
  fi
  if [ "$(grep -Ec "\"name\"[[:space:]]*:[[:space:]]*\"pi-agent-rust\"" "$gemini_settings")" -ne 1 ]; then
    echo "expected only custom-timeout pi-agent-rust Gemini hook to remain after second uninstall cleanup" >&2
    cat "$gemini_settings" >&2
    return 1
  fi
  if ! grep -Eq "\"timeout\"[[:space:]]*:[[:space:]]*7000" "$gemini_settings"; then
    echo "expected custom Gemini hook timeout to remain after second uninstall cleanup" >&2
    cat "$gemini_settings" >&2
    return 1
  fi
}

test_uninstall_uses_recorded_skill_paths() {
  local dir state_file recorded_codex managed_claude managed_codex
  dir="$(case_dir "uninstall-recorded-skill-paths")"
  recorded_codex="${dir}/home/custom-codex-home/skills/pi-agent-rust"

  managed_claude="${dir}/home/.claude/skills/pi-agent-rust/SKILL.md"
  managed_codex="${recorded_codex}/SKILL.md"
  mkdir -p "$(dirname "$managed_claude")" "$(dirname "$managed_codex")"

  cat > "$managed_claude" <<'SKILL'
<!-- pi_agent_rust installer managed skill -->
# Managed Claude skill
SKILL
  cat > "$managed_codex" <<'SKILL'
<!-- pi_agent_rust installer managed skill -->
# Managed Codex skill (recorded path)
SKILL

  state_file="${dir}/state/pi-agent-rust/install-state.env"
  mkdir -p "$(dirname "$state_file")"
  cat > "$state_file" <<STATE
PIAR_AGENT_SKILL_STATUS='installed (claude,codex)'
PIAR_AGENT_SKILL_CLAUDE_PATH='${dir}/home/.claude/skills/pi-agent-rust'
PIAR_AGENT_SKILL_CODEX_PATH='${recorded_codex}'
STATE

  run_uninstaller "$dir" --yes --no-gum

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Removed installer-managed skill: ${dir}/home/.claude/skills/pi-agent-rust"
  assert_output_contains "$dir" "Removed installer-managed skill: ${recorded_codex}"
  if [ -e "${dir}/home/.claude/skills/pi-agent-rust" ]; then
    echo "installer-managed Claude skill should be removed" >&2
    return 1
  fi
  if [ -e "${recorded_codex}" ]; then
    echo "installer-managed Codex skill at recorded path should be removed" >&2
    return 1
  fi
}

test_uninstall_skips_unexpected_skill_paths() {
  local dir state_file unexpected_dir unexpected_skill
  dir="$(case_dir "uninstall-skip-unexpected-skill-path")"
  unexpected_dir="${dir}/home/custom/pi-agent-rust"
  unexpected_skill="${unexpected_dir}/SKILL.md"
  mkdir -p "$unexpected_dir"

  cat > "$unexpected_skill" <<'SKILL'
<!-- pi_agent_rust installer managed skill -->
# Managed marker on unexpected path
SKILL

  state_file="${dir}/state/pi-agent-rust/install-state.env"
  mkdir -p "$(dirname "$state_file")"
  cat > "$state_file" <<STATE
PIAR_AGENT_SKILL_CODEX_PATH='${unexpected_dir}'
STATE

  run_uninstaller "$dir" --yes --no-gum

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Skipping unexpected skill directory path: ${unexpected_dir}"
  if [ ! -f "$unexpected_skill" ]; then
    echo "unexpected skill path should be preserved" >&2
    return 1
  fi
}

test_checksum_inline_success() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "checksum-inline-success")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Checksum verified for"
  assert_output_contains "$dir" "Checksum:  verified (--checksum)"
}

test_checksum_mismatch_fails_hard() {
  local dir artifact artifact_url wrong_checksum
  dir="$(case_dir "checksum-mismatch")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  wrong_checksum="0000000000000000000000000000000000000000000000000000000000000000"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${wrong_checksum}" \
    --no-completions

  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "Checksum mismatch"
  assert_output_contains "$dir" "Release checksum verification failed; aborting install"
}

test_checksum_missing_manifest_entry_fails_hard() {
  local dir artifact artifact_url checksum_manifest
  dir="$(case_dir "checksum-missing-entry")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"

  checksum_manifest="${dir}/fixtures/custom.sha256"
  cat > "$checksum_manifest" <<'MANIFEST'
1111111111111111111111111111111111111111111111111111111111111111  other-artifact
2222222222222222222222222222222222222222222222222222222222222222  another-artifact
MANIFEST

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum-url "file://${checksum_manifest}" \
    --no-completions

  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "No checksum entry found"
  assert_output_contains "$dir" "Release checksum verification failed; aborting install"
}

test_sigstore_bundle_unavailable_soft_skip() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "sigstore-bundle-unavailable")"
  write_existing_pi_stub "$dir"
  write_cosign_stub "$dir" "pass"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --no-completions

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Offline mode: skipping signature verification without --sigstore-bundle-url"
  assert_output_contains "$dir" "Signature: skipped (offline; bundle not provided)"
}

test_sigstore_cosign_failure_fails_hard() {
  local dir artifact artifact_url bundle checksum
  dir="$(case_dir "sigstore-cosign-fail")"
  write_existing_pi_stub "$dir"
  write_cosign_stub "$dir" "fail"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"
  bundle="${dir}/fixtures/pi-fixture.sigstore.json"
  printf '{"mediaType":"application/vnd.dev.sigstore.bundle+json;version=0.3"}\n' > "$bundle"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --sigstore-bundle-url "file://${bundle}" \
    --no-completions

  assert_exit_code "$dir" 1
  assert_output_contains "$dir" "Sigstore verification failed"
  assert_output_contains "$dir" "Release signature verification failed; aborting install"
}

test_sigstore_cosign_success() {
  local dir artifact artifact_url bundle checksum cosign_log
  dir="$(case_dir "sigstore-cosign-success")"
  write_existing_pi_stub "$dir"
  write_cosign_stub "$dir" "pass"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"
  bundle="${dir}/fixtures/pi-fixture.sigstore.json"
  cosign_log="${dir}/cosign.log"
  printf '{"mediaType":"application/vnd.dev.sigstore.bundle+json;version=0.3"}\n' > "$bundle"

  COSIGN_LOG_PATH="$cosign_log" run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --sigstore-bundle-url "file://${bundle}" \
    --no-completions

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Signature verified (cosign)"
  assert_output_contains "$dir" "Signature: verified"
  assert_file_contains "$cosign_log" "verify-blob"
  assert_file_contains "$cosign_log" "--bundle"
}

test_completions_unsupported_build_soft_skip() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "completions-unsupported")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "unsupported"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Shell completions: skipped (binary has no completion subcommand)"
  assert_output_contains "$dir" "Shell:     skipped (unsupported by this pi build)"
}

test_completions_generation_failure_recorded() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "completions-generation-fail")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "completion_fail"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Failed to generate bash completions"
  assert_output_contains "$dir" "Shell:     failed (completion generation error)"
}

test_completions_success_writes_file() {
  local dir artifact artifact_url checksum completion_file
  dir="$(case_dir "completions-success")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "completion_ok"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  completion_file="${dir}/data/bash-completion/completions/pi"

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Installed bash completions to"
  assert_output_contains "$dir" "Shell:     installed (bash)"
  if [ ! -f "$completion_file" ]; then
    echo "expected completion file: ${completion_file}" >&2
    return 1
  fi
  if ! grep -Fq "bash completion for pi fixture" "$completion_file"; then
    echo "completion file missing expected content: ${completion_file}" >&2
    cat "$completion_file" >&2
    return 1
  fi
}

test_completions_help_discovery_path_succeeds() {
  local dir artifact artifact_url checksum completion_file
  dir="$(case_dir "completions-help-discovery")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "help_lists_completions"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  completion_file="${dir}/data/bash-completion/completions/pi"
  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Installed bash completions to"
  assert_output_contains "$dir" "Shell:     installed (bash)"
  [ -f "$completion_file" ] || { echo "expected completion file: ${completion_file}" >&2; return 1; }
}

test_completions_help_inconclusive_falls_back_to_probe() {
  local dir artifact artifact_url checksum completion_file
  dir="$(case_dir "completions-help-inconclusive")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "help_inconclusive_probe_ok"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  completion_file="${dir}/data/bash-completion/completions/pi"
  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Installed bash completions to"
  assert_output_contains "$dir" "Shell:     installed (bash)"
  [ -f "$completion_file" ] || { echo "expected completion file: ${completion_file}" >&2; return 1; }
}

test_completions_help_conclusive_no_command_skips_fast() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "completions-help-conclusive-no-command")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "help_conclusive_no_completion"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  PI_INSTALLER_COMPLETION_PROBE_TIMEOUT=1 \
  STUB_COMPLETION_SLEEP_SECS=3 \
  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Shell completions: skipped (binary has no completion subcommand)"
  assert_output_contains "$dir" "Shell:     skipped (unsupported by this pi build)"
}

test_completions_internal_timeout_fallback_succeeds() {
  local dir artifact artifact_url checksum completion_file
  dir="$(case_dir "completions-internal-timeout-fallback")"
  write_existing_pi_stub "$dir"
  write_timeout_unusable_stubs "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "help_lists_completions"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  completion_file="${dir}/data/bash-completion/completions/pi"
  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Installed bash completions to"
  assert_output_contains "$dir" "Shell:     installed (bash)"
  [ -f "$completion_file" ] || { echo "expected completion file: ${completion_file}" >&2; return 1; }
}

test_completions_probe_timeout_is_non_fatal() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "completions-probe-timeout")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "completion_probe_hang"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  PI_INSTALLER_COMPLETION_PROBE_TIMEOUT=1 \
  STUB_COMPLETION_SLEEP_SECS=3 \
  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Shell completions probe timed out; skipping completion installation"
  assert_output_contains "$dir" "Shell:     failed (completion probe timed out)"
}

test_completions_generation_timeout_is_non_fatal() {
  local dir artifact artifact_url checksum
  dir="$(case_dir "completions-generation-timeout")"
  write_existing_pi_stub "$dir"

  artifact="${dir}/fixtures/pi-fixture"
  write_artifact_binary "$artifact" "completion_hang"
  artifact_url="file://${artifact}"
  checksum="$(sha256_file "$artifact")"

  PI_INSTALLER_COMPLETION_CMD_TIMEOUT=1 \
  STUB_COMPLETION_SLEEP_SECS=3 \
  run_installer "$dir" \
    --yes --no-gum --offline \
    --version v9.9.9 \
    --dest "${dir}/dest" \
    --artifact-url "${artifact_url}" \
    --checksum "${checksum}" \
    --completions bash

  assert_exit_code "$dir" 0
  assert_output_contains "$dir" "Failed to generate bash completions (timed out)"
  assert_output_contains "$dir" "Shell:     failed (completion generation timed out)"
}

main() {
  if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
  fi

  run_test test_help_lists_installer_flags
  run_test test_release_workflows_do_not_use_no_verify
  run_test test_skill_smoke_script_passes
  run_test test_invalid_completions_value_fails
  run_test test_unknown_option_fails
  run_test test_missing_option_value_fails
  run_test test_missing_option_value_when_next_arg_is_flag_fails
  run_test test_custom_artifact_download_failure_does_not_source_fallback_without_version
  run_test test_offline_tarball_mode_installs_local_artifact
  run_test test_offline_mode_blocks_network_artifact_urls
  run_test test_offline_relative_tarball_path_is_accepted
  run_test test_proxy_args_are_applied_to_curl_downloads
  run_test test_linux_target_uses_supported_linux_artifact_naming
  run_test test_rosetta_prefers_arm64_artifact_naming
  run_test test_wsl_detection_warning_is_emitted
  run_test test_installer_creates_rpi_alias_when_available
  run_test test_installer_skips_rpi_alias_when_existing_command_present
  run_test test_legacy_agent_settings_cleanup_is_safe_and_idempotent
  run_test test_legacy_cleanup_skips_unexpected_settings_paths
  run_test test_agent_skills_install_by_default
  run_test test_agent_skill_install_ignores_shadow_pwd_skill
  run_test test_no_agent_skills_opt_out
  run_test test_existing_custom_skill_dirs_are_not_overwritten
  run_test test_skill_copy_failure_preserves_existing_managed_skills
  run_test test_skill_custom_plus_copy_failure_reports_partial
  run_test test_uninstall_removes_only_installer_managed_skills
  run_test test_uninstall_removes_recorded_rpi_alias
  run_test test_uninstall_cleans_legacy_agent_settings_hooks
  run_test test_uninstall_uses_recorded_skill_paths
  run_test test_uninstall_skips_unexpected_skill_paths
  run_test test_checksum_inline_success
  run_test test_checksum_mismatch_fails_hard
  run_test test_checksum_missing_manifest_entry_fails_hard
  run_test test_sigstore_bundle_unavailable_soft_skip
  run_test test_sigstore_cosign_failure_fails_hard
  run_test test_sigstore_cosign_success
  run_test test_completions_unsupported_build_soft_skip
  run_test test_completions_generation_failure_recorded
  run_test test_completions_success_writes_file
  run_test test_completions_help_discovery_path_succeeds
  run_test test_completions_help_inconclusive_falls_back_to_probe
  run_test test_completions_help_conclusive_no_command_skips_fast
  run_test test_completions_internal_timeout_fallback_succeeds
  run_test test_completions_probe_timeout_is_non_fatal
  run_test test_completions_generation_timeout_is_non_fatal

  echo ""
  echo "work dir: ${WORK_ROOT}"
  echo "passed:   ${PASS_COUNT}"
  echo "failed:   ${FAIL_COUNT}"

  if [ "${FAIL_COUNT}" -gt 0 ]; then
    exit 1
  fi
}

main "$@"
