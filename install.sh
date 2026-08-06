#!/usr/bin/env bash
#
# pi_agent_rust installer
#
# One-liner install:
#   curl -fsSL "https://raw.githubusercontent.com/Dicklesworthstone/pi_agent_rust/main/install.sh?$(date +%s)" | bash
#
# Highlights:
# - Installs latest (or requested) GitHub release binary for your platform
# - Verifies artifact checksum via SHA256SUMS
# - Detects existing TypeScript pi and can migrate to Rust canonical `pi`
# - Creates `legacy-pi` alias for the preserved TypeScript CLI when migrated
# - Writes installer state for idempotent re-runs and clean uninstall

set -euo pipefail
umask 022
shopt -s lastpipe 2>/dev/null || true

OWNER="${OWNER:-Dicklesworthstone}"
REPO="${REPO:-pi_agent_rust}"
VERSION="${VERSION:-}"

DEST_DEFAULT="$HOME/.local/bin"
DEST="$DEST_DEFAULT"
DEST_EXPLICIT=0
SYSTEM=0

EASY=0
YES=0
QUIET=0
NO_GUM=0
FROM_SOURCE=0
VERIFY=0
NO_VERIFY=0
FORCE_INSTALL=0
OFFLINE="${PI_INSTALLER_OFFLINE:-0}"
OFFLINE_TARBALL="${PI_INSTALLER_OFFLINE_TARBALL:-}"
AGENT_SKILLS_ENABLED="${AGENT_SKILLS_ENABLED:-1}"
RETAIN_TEMP="${PI_INSTALLER_RETAIN_TEMP:-0}"

CHECKSUM="${CHECKSUM:-}"
CHECKSUM_URL="${CHECKSUM_URL:-}"
ARTIFACT_URL="${ARTIFACT_URL:-}"
SOURCE_DIR="${SOURCE_DIR:-}"
SIGSTORE_BUNDLE_URL="${SIGSTORE_BUNDLE_URL:-}"
COSIGN_IDENTITY_RE="${COSIGN_IDENTITY_RE:-^https://github.com/${OWNER}/${REPO}/.github/workflows/release.yml@refs/tags/.*$}"
COSIGN_OIDC_ISSUER="${COSIGN_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"
COMPLETIONS_MODE="${COMPLETIONS_MODE:-auto}"

PROXY_ARGS=()
PROXY_SOURCE=""
WSL_DETECTED=0

# ask|yes|no
ADOPT_MODE="ask"
LEGACY_ALIAS_NAME="${LEGACY_ALIAS_NAME:-legacy-pi}"
COMPAT_ALIAS_NAME="${COMPAT_ALIAS_NAME:-rpi}"

OS=""
ARCH=""
TARGET=""
EXE_EXT=""
ASSET_PLATFORM=""
ASSET_NAME=""
SHA_URL=""

CURRENT_PI_PATH=""
CURRENT_PI_VERSION=""
TS_PI_DETECTED=0
ADOPT_TS=0
ADOPT_CANONICAL=0

FINAL_BIN_NAME="pi"
INSTALL_BIN_PATH=""

LEGACY_ALIAS_PATH=""
LEGACY_TARGET_PATH=""
LEGACY_MOVED_FROM=""
LEGACY_MOVED_TO=""
COMPAT_ALIAS_PATH=""
COMPAT_ALIAS_STATUS="pending"

PATH_MARKER="# pi-agent-rust installer PATH"
PATH_UPDATED_FILES=""

AGENT_SKILL_NAME="pi-agent-rust"
AGENT_SKILL_STATUS="pending"
AGENT_SKILL_CLAUDE_PATH=""
AGENT_SKILL_CODEX_PATH=""
AGENT_SKILL_MARKER="pi_agent_rust installer managed skill"

STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/pi-agent-rust"
STATE_FILE="$STATE_DIR/install-state.env"
STATE_VERSION="1"

TMP=""
LOCK_DIR="${PI_INSTALLER_LOCK_DIR:-/tmp/pi-agent-rust-install.lock.d}"
LOCKED=0
MIGRATION_MOVED=0
INSTALL_COMMITTED=0
INSTALL_SOURCE="release"
CHECKSUM_STATUS="pending"
SIGSTORE_STATUS="pending"
COMPLETIONS_STATUS="pending"

HAS_GUM=0
if command -v gum >/dev/null 2>&1 && [ -t 1 ]; then
  HAS_GUM=1
fi

log() {
  [ "$QUIET" -eq 1 ] && return 0
  echo -e "$*" >&2
}

info() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 39 "→ $*" >&2
  else
    echo -e "\033[0;34m→\033[0m $*" >&2
  fi
}

ok() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 42 "✓ $*" >&2
  else
    echo -e "\033[0;32m✓\033[0m $*" >&2
  fi
}

warn() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 214 "⚠ $*" >&2
  else
    echo -e "\033[1;33m⚠\033[0m $*" >&2
  fi
}

err() {
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 196 "✗ $*" >&2
  else
    echo -e "\033[0;31m✗\033[0m $*" >&2
  fi
}

run_with_spinner() {
  local title="$1"
  shift
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ] && [ "$QUIET" -eq 0 ] && [ -t 1 ]; then
    gum spin --spinner dot --title "$title" -- "$@"
  else
    # Keep status text off stdout so callers can safely capture command output.
    info "$title" >&2
    "$@"
  fi
}

version_timeout_cmd() {
  if command -v timeout >/dev/null 2>&1; then
    printf '%s\n' "timeout"
    return 0
  fi
  if command -v gtimeout >/dev/null 2>&1; then
    printf '%s\n' "gtimeout"
    return 0
  fi
  printf '%s\n' ""
}

run_command_with_timeout_capture() {
  local timeout_seconds="$1"
  shift

  local timeout_cmd=""
  timeout_cmd="$(version_timeout_cmd)"

  if [ -n "$timeout_cmd" ]; then
    if "$timeout_cmd" "$timeout_seconds" "$@"; then
      return 0
    else
      local timeout_rc=$?
      # Broken timeout wrappers may return 127; fall back to internal timeout handling.
      if [ "$timeout_rc" -ne 127 ]; then
        return "$timeout_rc"
      fi
    fi
  fi

  local out_file=""
  out_file="$(mktemp 2>/dev/null || true)"
  if [ -z "$out_file" ]; then
    return 125
  fi

  local timed_out_file=""
  timed_out_file="$(mktemp 2>/dev/null || true)"
  if [ -z "$timed_out_file" ]; then
    rm -f "$out_file" 2>/dev/null || true
    return 125
  fi
  : > "$timed_out_file"

  "$@" > "$out_file" 2>/dev/null &
  local cmd_pid=$!

  (
    sleep "$timeout_seconds"
    if kill -0 "$cmd_pid" 2>/dev/null; then
      printf '1\n' > "$timed_out_file"
      kill "$cmd_pid" >/dev/null 2>&1 || true
      sleep 1
      kill -9 "$cmd_pid" >/dev/null 2>&1 || true
    fi
  ) &
  local watchdog_pid=$!

  local cmd_rc=0
  if wait "$cmd_pid" 2>/dev/null; then
    cmd_rc=0
  else
    cmd_rc=$?
  fi

  kill "$watchdog_pid" >/dev/null 2>&1 || true
  wait "$watchdog_pid" 2>/dev/null || true

  local timed_out=0
  if [ -s "$timed_out_file" ]; then
    timed_out=1
  fi

  if [ -f "$out_file" ]; then
    cat "$out_file"
    rm -f "$out_file" 2>/dev/null || true
  fi
  rm -f "$timed_out_file" 2>/dev/null || true

  if [ "$timed_out" -eq 1 ]; then
    return 124
  fi
  return "$cmd_rc"
}

capture_version_line() {
  local bin_path="$1"
  local out=""
  out="$(run_command_with_timeout_capture 2 "$bin_path" --version || true)"
  out="$(printf '%s\n' "$out" | head -1)"
  printf '%s\n' "$out"
}

is_managed_alias() {
  local path="$1"
  [ -f "$path" ] || return 1
  grep -q "pi_agent_rust installer managed alias" "$path" 2>/dev/null
}

setup_proxy() {
  PROXY_ARGS=()
  PROXY_SOURCE=""

  local https_proxy_value="${HTTPS_PROXY:-${https_proxy:-}}"
  local http_proxy_value="${HTTP_PROXY:-${http_proxy:-}}"

  if [ -n "$https_proxy_value" ]; then
    PROXY_ARGS=(--proxy "$https_proxy_value")
    PROXY_SOURCE="$https_proxy_value"
    info "Using HTTPS proxy from environment"
    return 0
  fi

  if [ -n "$http_proxy_value" ]; then
    PROXY_ARGS=(--proxy "$http_proxy_value")
    PROXY_SOURCE="$http_proxy_value"
    info "Using HTTP proxy from environment"
    return 0
  fi
}

is_local_resource_ref() {
  local ref="${1:-}"
  if [ -z "$ref" ]; then
    return 1
  fi
  if [[ "$ref" != *"://"* ]]; then
    # Plain paths (relative or absolute) are treated as local filesystem refs.
    return 0
  fi
  case "$ref" in
    file://*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

resource_to_local_path() {
  local ref="$1"
  case "$ref" in
    file://*)
      local path="${ref#file://}"
      path="${path%%\?*}"
      path="${path%%#*}"
      printf '%s\n' "$path"
      ;;
    *)
      printf '%s\n' "$ref"
      ;;
  esac
}

redact_proxy_value() {
  local raw="${1:-}"
  if [ -z "$raw" ]; then
    printf '%s\n' "$raw"
    return 0
  fi

  if [[ "$raw" == *"://"* ]]; then
    local scheme="${raw%%://*}"
    local rest="${raw#*://}"
    local host="${rest##*@}"
    if [ "$host" != "$rest" ]; then
      printf '%s\n' "${scheme}://***@${host}"
      return 0
    fi
  fi

  if [[ "$raw" == *"@"* ]] && [[ "$raw" == *":"* ]]; then
    local host="${raw##*@}"
    printf '%s\n' "***@${host}"
    return 0
  fi

  printf '%s\n' "$raw"
}

ensure_network_allowed() {
  local url="$1"
  local context="$2"
  if [ "$OFFLINE" -eq 1 ] && ! is_local_resource_ref "$url"; then
    err "Offline mode forbids network access for ${context}: $url"
    return 1
  fi
  return 0
}

fetch_url_to_file() {
  local url="$1"
  local output_path="$2"
  local context="${3:-resource}"
  local connect_timeout="${PI_INSTALLER_CONNECT_TIMEOUT:-10}"
  local max_time="${PI_INSTALLER_MAX_TIME:-180}"
  local retries="${PI_INSTALLER_RETRIES:-2}"
  local retry_delay="${PI_INSTALLER_RETRY_DELAY:-1}"

  if ! ensure_network_allowed "$url" "$context"; then
    return 1
  fi

  case "$context" in
    "agent skill")
      connect_timeout="${PI_INSTALLER_AGENT_SKILL_CONNECT_TIMEOUT:-3}"
      max_time="${PI_INSTALLER_AGENT_SKILL_MAX_TIME:-8}"
      retries="${PI_INSTALLER_AGENT_SKILL_RETRIES:-0}"
      ;;
    "release artifact")
      connect_timeout="${PI_INSTALLER_ARTIFACT_CONNECT_TIMEOUT:-10}"
      max_time="${PI_INSTALLER_ARTIFACT_MAX_TIME:-240}"
      retries="${PI_INSTALLER_ARTIFACT_RETRIES:-2}"
      ;;
    "release checksum manifest"|"checksum file"|"derived checksum file"|"sigstore bundle")
      connect_timeout="${PI_INSTALLER_META_CONNECT_TIMEOUT:-5}"
      max_time="${PI_INSTALLER_META_MAX_TIME:-20}"
      retries="${PI_INSTALLER_META_RETRIES:-2}"
      ;;
  esac

  if is_local_resource_ref "$url"; then
    local local_path
    local_path="$(resource_to_local_path "$url")"
    if [ ! -e "$local_path" ]; then
      err "Local ${context} not found: $local_path"
      return 1
    fi
    cp "$local_path" "$output_path"
    return 0
  fi

  curl -fsSL ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} \
    --connect-timeout "$connect_timeout" \
    --max-time "$max_time" \
    --retry "$retries" \
    --retry-delay "$retry_delay" \
    --retry-connrefused \
    "$url" -o "$output_path"
}

fetch_url_to_stdout() {
  local url="$1"
  local context="${2:-resource}"
  local connect_timeout="${PI_INSTALLER_CONNECT_TIMEOUT:-10}"
  local max_time="${PI_INSTALLER_MAX_TIME:-180}"
  local retries="${PI_INSTALLER_RETRIES:-2}"
  local retry_delay="${PI_INSTALLER_RETRY_DELAY:-1}"

  if ! ensure_network_allowed "$url" "$context"; then
    return 1
  fi

  case "$context" in
    "agent skill")
      connect_timeout="${PI_INSTALLER_AGENT_SKILL_CONNECT_TIMEOUT:-3}"
      max_time="${PI_INSTALLER_AGENT_SKILL_MAX_TIME:-8}"
      retries="${PI_INSTALLER_AGENT_SKILL_RETRIES:-0}"
      ;;
    "release checksum manifest"|"checksum file"|"derived checksum file"|"sigstore bundle")
      connect_timeout="${PI_INSTALLER_META_CONNECT_TIMEOUT:-5}"
      max_time="${PI_INSTALLER_META_MAX_TIME:-20}"
      retries="${PI_INSTALLER_META_RETRIES:-2}"
      ;;
  esac

  if is_local_resource_ref "$url"; then
    local local_path
    local_path="$(resource_to_local_path "$url")"
    if [ ! -f "$local_path" ]; then
      err "Local ${context} not found: $local_path"
      return 1
    fi
    cat "$local_path"
    return 0
  fi

  curl -fsSL ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} \
    --connect-timeout "$connect_timeout" \
    --max-time "$max_time" \
    --retry "$retries" \
    --retry-delay "$retry_delay" \
    --retry-connrefused \
    "$url"
}

fetch_effective_url() {
  local url="$1"
  local context="${2:-resource}"
  local connect_timeout="${PI_INSTALLER_CONNECT_TIMEOUT:-10}"
  local max_time="${PI_INSTALLER_MAX_TIME:-180}"
  local retries="${PI_INSTALLER_RETRIES:-2}"
  local retry_delay="${PI_INSTALLER_RETRY_DELAY:-1}"

  if ! ensure_network_allowed "$url" "$context"; then
    return 1
  fi

  case "$context" in
    "agent skill")
      connect_timeout="${PI_INSTALLER_AGENT_SKILL_CONNECT_TIMEOUT:-3}"
      max_time="${PI_INSTALLER_AGENT_SKILL_MAX_TIME:-8}"
      retries="${PI_INSTALLER_AGENT_SKILL_RETRIES:-0}"
      ;;
    "release checksum manifest"|"checksum file"|"derived checksum file"|"sigstore bundle")
      connect_timeout="${PI_INSTALLER_META_CONNECT_TIMEOUT:-5}"
      max_time="${PI_INSTALLER_META_MAX_TIME:-20}"
      retries="${PI_INSTALLER_META_RETRIES:-2}"
      ;;
  esac

  if is_local_resource_ref "$url"; then
    printf '%s\n' "$url"
    return 0
  fi

  curl -fsSL -o /dev/null -w '%{url_effective}' ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} \
    --connect-timeout "$connect_timeout" \
    --max-time "$max_time" \
    --retry "$retries" \
    --retry-delay "$retry_delay" \
    --retry-connrefused \
    "$url"
}

probe_url_head() {
  local url="$1"
  local context="${2:-resource}"

  if ! ensure_network_allowed "$url" "$context"; then
    return 1
  fi

  if is_local_resource_ref "$url"; then
    local local_path
    local_path="$(resource_to_local_path "$url")"
    [ -e "$local_path" ]
    return $?
  fi

  curl -fsSLI ${PROXY_ARGS[@]+"${PROXY_ARGS[@]}"} --connect-timeout 5 --max-time 10 "$url" >/dev/null 2>&1
}

remove_path_recursively() {
  local target="$1"

  if [ -z "$target" ]; then
    return 1
  fi
  if [ ! -e "$target" ] && [ ! -L "$target" ]; then
    return 0
  fi

  if [ -L "$target" ] || [ -f "$target" ] || [ -p "$target" ] || [ -S "$target" ] || [ -b "$target" ] || [ -c "$target" ]; then
    rm -f "$target"
    return $?
  fi

  if [ -d "$target" ]; then
    local child=""
    while IFS= read -r -d '' child; do
      remove_path_recursively "$child" || return 1
    done < <(find "$target" -mindepth 1 -maxdepth 1 -print0 2>/dev/null)
    rmdir "$target" 2>/dev/null || return 1
    return 0
  fi

  return 1
}

pi_ascii_logo() {
  cat <<'ASCII'
                                                                 
                                                                 
   ██████╗ ██╗     █████╗  ██████╗ ███████╗███╗   ██╗████████╗   
   ██╔══██╗██║    ██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝   
   ██████╔╝██║    ███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║      
   ██╔═══╝ ██║    ██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║      
   ██║     ██║    ██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║      
   ╚═╝     ╚═╝    ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝      
                                                                 
                                             /\                  
                                            ( /   @ @    ()      
      ██████╗ ██╗   ██╗███████╗████████╗     \  __| |__  /       
      ██╔══██╗██║   ██║██╔════╝╚══██╔══╝      -/   "   \-        
      ██████╔╝██║   ██║███████╗   ██║        /-|       |-\       
      ██╔══██╗██║   ██║╚════██║   ██║       / /-\     /-\ \      
      ██║  ██║╚██████╔╝███████║   ██║        / /-`---'-\\ \      
      ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝          /         \        
ASCII
}

pi_ascii_logo_normalized() {
  local logo="$1"
  local line=""
  local max_width=0

  while IFS= read -r line; do
    if [ "${#line}" -gt "$max_width" ]; then
      max_width="${#line}"
    fi
  done <<< "$logo"

  while IFS= read -r line; do
    printf '%-*s\n' "$max_width" "$line"
  done <<< "$logo"
}

pi_ascii_logo_gum() {
  local logo="$1"
  local line=""
  local idx=0
  local styled=""

  while IFS= read -r line; do
    local color=51
    case "$idx" in
      0|1) color=159 ;;
      2) color=123 ;;
      3) color=117 ;;
      4) color=111 ;;
      5) color=75 ;;
      6) color=69 ;;
      7) color=63 ;;
      8) color=69 ;;
      9|10) color=75 ;;
      11) color=160 ;;
      12) color=166 ;;
      13) color=172 ;;
      14) color=178 ;;
      15) color=208 ;;
      16) color=214 ;;
    esac
    local rendered
    rendered="$(gum style --foreground "$color" --bold "$line")"
    if [ -z "$styled" ]; then
      styled="$rendered"
    else
      styled="${styled}"$'\n'"$rendered"
    fi
    idx=$((idx + 1))
  done <<< "$logo"
  printf '%s\n' "$styled"
}

pi_ascii_logo_ansi() {
  local logo="$1"
  local line=""
  local idx=0

  while IFS= read -r line; do
    local color=51
    case "$idx" in
      0|1) color=159 ;;
      2) color=123 ;;
      3) color=117 ;;
      4) color=111 ;;
      5) color=75 ;;
      6) color=69 ;;
      7) color=63 ;;
      8) color=69 ;;
      9|10) color=75 ;;
      11) color=160 ;;
      12) color=166 ;;
      13) color=172 ;;
      14) color=178 ;;
      15) color=208 ;;
      16) color=214 ;;
    esac
    printf '\033[1;38;5;%sm%s\033[0m\n' "$color" "$line"
    idx=$((idx + 1))
  done <<< "$logo"
}

usage() {
  cat <<'USAGE'
Usage: install.sh [options]

Options:
  --version vX.Y.Z       Install a specific release tag
  --dest DIR             Install directory (default: ~/.local/bin)
  --system               Install to /usr/local/bin
  --easy-mode            Add install dir to PATH in shell rc files
  --artifact-url URL     Install from explicit release artifact URL
  --checksum HEX         Expected SHA256 for explicit artifact
  --checksum-url URL     URL to checksum file or manifest
  --sigstore-bundle-url URL
                          URL to Sigstore bundle (.sigstore.json)
  --from-source          Build from source instead of downloading release binary
  --source-dir DIR       Build from a local pi_agent_rust checkout (implies
                          --from-source). Useful when iterating on a feature
                          branch, the desired commit is not tagged yet, or
                          a platform has no prebuilt binary (e.g. FreeBSD).
                          Compatible with --offline because no network is
                          needed once the source is on disk.
  --verify               Run `pi --version` after install
  --no-verify            Skip checksum + signature verification
  --offline [TARBALL]    Offline mode; optional local artifact path
  --completions SHELL    Install shell completions for auto|off|bash|zsh|fish
  --no-completions       Skip shell completion installation
  --no-agent-skills      Skip installing AI agent skill files for Claude/Codex
  --yes, -y              Non-interactive yes to prompts
  --adopt                Auto-adopt Rust as canonical `pi` when TS pi is detected
  --keep-existing-pi     Do not replace existing `pi`; install as `pi-rust`
  --legacy-alias NAME    Alias name for migrated TypeScript pi (default: legacy-pi)
  --force                Reinstall even if same version is already installed
  --quiet, -q            Suppress non-error output
  --no-gum               Disable gum formatting
  -h, --help             Show this help
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --version requires a value"
        usage
        exit 1
      fi
      VERSION="$2"
      shift 2
      ;;
    --dest)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --dest requires a value"
        usage
        exit 1
      fi
      DEST="$2"
      DEST_EXPLICIT=1
      shift 2
      ;;
    --system)
      SYSTEM=1
      DEST="/usr/local/bin"
      DEST_EXPLICIT=1
      shift
      ;;
    --easy-mode)
      EASY=1
      shift
      ;;
    --artifact-url)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --artifact-url requires a value"
        usage
        exit 1
      fi
      ARTIFACT_URL="$2"
      shift 2
      ;;
    --checksum)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --checksum requires a value"
        usage
        exit 1
      fi
      CHECKSUM="$2"
      shift 2
      ;;
    --checksum-url)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --checksum-url requires a value"
        usage
        exit 1
      fi
      CHECKSUM_URL="$2"
      shift 2
      ;;
    --sigstore-bundle-url)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --sigstore-bundle-url requires a value"
        usage
        exit 1
      fi
      SIGSTORE_BUNDLE_URL="$2"
      shift 2
      ;;
    --from-source)
      FROM_SOURCE=1
      shift
      ;;
    --source-dir)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --source-dir requires a path argument"
        usage
        exit 1
      fi
      SOURCE_DIR="$2"
      FROM_SOURCE=1
      shift 2
      ;;
    --verify)
      VERIFY=1
      shift
      ;;
    --no-verify)
      NO_VERIFY=1
      shift
      ;;
    --offline)
      OFFLINE=1
      if [ $# -ge 2 ] && [[ "$2" != -* ]]; then
        OFFLINE_TARBALL="$2"
        shift 2
      else
        shift
      fi
      ;;
    --completions)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --completions requires a value"
        usage
        exit 1
      fi
      COMPLETIONS_MODE="$2"
      shift 2
      ;;
    --no-completions)
      COMPLETIONS_MODE="off"
      shift
      ;;
    --no-agent-skills)
      AGENT_SKILLS_ENABLED=0
      shift
      ;;
    --yes|-y)
      YES=1
      shift
      ;;
    --adopt)
      ADOPT_MODE="yes"
      shift
      ;;
    --keep-existing-pi)
      ADOPT_MODE="no"
      shift
      ;;
    --legacy-alias)
      if [ $# -lt 2 ] || [[ "$2" == -* ]]; then
        err "Option --legacy-alias requires a value"
        usage
        exit 1
      fi
      LEGACY_ALIAS_NAME="$2"
      shift 2
      ;;
    --force)
      FORCE_INSTALL=1
      shift
      ;;
    --quiet|-q)
      QUIET=1
      shift
      ;;
    --no-gum)
      NO_GUM=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      err "Unknown option: $1"
      usage
      exit 1
      ;;
  esac
done

show_header() {
  [ "$QUIET" -eq 1 ] && return 0
  local logo
  local header_version
  local header_indent="   "
  if [ -n "$VERSION" ]; then
    header_version="$VERSION"
  else
    header_version="latest (auto)"
  fi
  logo="$(pi_ascii_logo)"
  logo="$(pi_ascii_logo_normalized "$logo")"

  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    local styled_logo
    styled_logo="$(pi_ascii_logo_gum "$logo")"
    styled_logo="${styled_logo}"$'\n'
    gum style \
      --border double \
      --border-foreground 45 \
      --padding "1 3" \
      --margin "1 0" \
      "$styled_logo" \
      "$(gum style --foreground 51 --bold "${header_indent}Pi Agent Rust Installer")" \
      "$(gum style --foreground 226 --bold "${header_indent}Install target version: ${header_version}")" \
      "$(gum style --foreground 252 "${header_indent}Based on Pi Agent by Mario Zechner")" \
      "$(gum style --foreground 252 "${header_indent}Rust version by Jeffrey Emanuel")" \
      "$(gum style --foreground 248 "${header_indent}Fast Rust-native coding agent installer")" \
      "$(gum style --foreground 248 "${header_indent}Checksum verification by default | Optional Sigstore/cosign")" \
      "$(gum style --foreground 111 "${header_indent}Repository: ${OWNER}/${REPO}")"
  else
    echo ""
    pi_ascii_logo_ansi "$logo"
    echo ""
    echo -e "\033[1;38;5;51m${header_indent}Pi Agent Rust Installer\033[0m"
    echo -e "\033[1;38;5;226m${header_indent}Install target version: ${header_version}\033[0m"
    echo -e "\033[0;38;5;252m${header_indent}Based on Pi Agent by Mario Zechner\033[0m"
    echo -e "\033[0;38;5;252m${header_indent}Rust version by Jeffrey Emanuel\033[0m"
    echo -e "\033[0;38;5;248m${header_indent}Fast Rust-native coding agent installer\033[0m"
    echo -e "\033[0;38;5;248m${header_indent}Checksum verification by default | Optional Sigstore/cosign\033[0m"
    echo -e "\033[0;38;5;111m${header_indent}Repository: ${OWNER}/${REPO}\033[0m"
    echo ""
  fi
}

prompt_confirm() {
  local prompt="$1"
  local default_yes="${2:-0}"

  if [ "$YES" -eq 1 ]; then
    return 0
  fi

  if [ ! -t 0 ]; then
    if [ "$default_yes" -eq 1 ]; then
      return 0
    fi
    return 1
  fi

  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum confirm "$prompt"
    return $?
  fi

  local suffix="[y/N]"
  if [ "$default_yes" -eq 1 ]; then
    suffix="[Y/n]"
  fi

  printf "%s %s " "$prompt" "$suffix"
  local ans
  read -r ans || true
  if [ -z "$ans" ]; then
    if [ "$default_yes" -eq 1 ]; then
      return 0
    fi
    return 1
  fi
  case "$ans" in
    y|Y|yes|YES|Yes)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

normalize_version() {
  if [ -z "$VERSION" ]; then
    return 0
  fi
  if [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.].+)?$ ]]; then
    VERSION="v$VERSION"
  fi
}

resolve_version() {
  normalize_version
  if [ -n "$VERSION" ]; then
    return 0
  fi

  if [ -n "$ARTIFACT_URL" ] && [ "$FROM_SOURCE" -eq 0 ]; then
    VERSION="custom-artifact"
    info "Using custom artifact URL; skipping release tag resolution"
    return 0
  fi

  if [ "$OFFLINE" -eq 1 ]; then
    err "Offline mode requires --version or --offline <local-artifact>"
    exit 1
  fi

  info "Resolving latest release tag"
  local latest_url="https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"
  local tag=""
  if command -v curl >/dev/null 2>&1; then
    tag=$(fetch_url_to_stdout "$latest_url" "release metadata" 2>/dev/null \
      | grep '"tag_name":' \
      | sed -E 's/.*"([^"]+)".*/\1/' \
      || true)
    if [ -z "$tag" ]; then
      local redirect_target=""
      redirect_target=$(fetch_effective_url "https://github.com/${OWNER}/${REPO}/releases/latest" "release redirect" 2>/dev/null || true)
      if [[ "$redirect_target" =~ /tag/([^/?#]+) ]]; then
        tag="${BASH_REMATCH[1]}"
      fi
    fi
  fi

  if [ -z "$tag" ]; then
    err "Failed to resolve latest release tag"
    err "Pass --version vX.Y.Z or check network connectivity"
    exit 1
  fi

  VERSION="$tag"
  ok "Resolved ${VERSION}"
}

detect_platform() {
  OS=$(uname -s | tr '[:upper:]' '[:lower:]')
  ARCH=$(uname -m)

  if [ "$OS" = "darwin" ] && [ "$ARCH" = "x86_64" ]; then
    local darwin_arm64_capable=""
    darwin_arm64_capable=$(sysctl -in hw.optional.arm64 2>/dev/null || true)
    if [ "$darwin_arm64_capable" = "1" ]; then
      local darwin_translated=""
      darwin_translated=$(sysctl -in sysctl.proc_translated 2>/dev/null || true)
      ARCH="arm64"
      if [ "$darwin_translated" = "1" ]; then
        info "Rosetta shell detected on Apple Silicon; preferring native arm64 binary"
      else
        info "Apple Silicon host detected; preferring native arm64 binary"
      fi
    fi
  fi

  case "$ARCH" in
    x86_64|amd64)
      ARCH="x86_64"
      ;;
    arm64|aarch64)
      ARCH="aarch64"
      ;;
  esac

  TARGET=""
  EXE_EXT=""

  if [ "$OS" = "linux" ]; then
    if [ "${PI_INSTALLER_TEST_FORCE_WSL:-0}" = "1" ] \
      || grep -qi microsoft /proc/version 2>/dev/null \
      || grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null; then
      WSL_DETECTED=1
      warn "WSL detected; terminal/path integration may need extra configuration"
    fi
  fi

  case "${OS}-${ARCH}" in
    linux-x86_64)
      TARGET="x86_64-unknown-linux-gnu"
      ASSET_PLATFORM="linux-amd64"
      ;;
    linux-aarch64)
      TARGET="aarch64-unknown-linux-gnu"
      ASSET_PLATFORM="linux-arm64"
      ;;
    darwin-x86_64)
      TARGET="x86_64-apple-darwin"
      ASSET_PLATFORM="darwin-amd64"
      ;;
    darwin-aarch64)
      TARGET="aarch64-apple-darwin"
      ASSET_PLATFORM="darwin-arm64"
      ;;
    msys_nt*-x86_64|mingw*-x86_64|cygwin_nt*-x86_64)
      TARGET="x86_64-pc-windows-msvc"
      ASSET_PLATFORM="windows-amd64"
      EXE_EXT=".exe"
      ;;
    *)
      ;;
  esac

  if [ -z "$TARGET" ] && [ "$FROM_SOURCE" -eq 0 ]; then
    warn "No prebuilt binary published for ${OS}/${ARCH}; switching to --from-source"
    FROM_SOURCE=1
  fi
}

prepare_asset_urls() {
  if [ "$FROM_SOURCE" -eq 1 ]; then
    return 0
  fi

  if [ -n "$ARTIFACT_URL" ]; then
    local cleaned="${ARTIFACT_URL%%\?*}"
    cleaned="${cleaned%%#*}"
    ASSET_NAME="$(basename "$cleaned")"
    if [ -z "$ASSET_NAME" ] || [ "$ASSET_NAME" = "/" ] || [ "$ASSET_NAME" = "." ]; then
      err "Could not infer artifact name from --artifact-url: $ARTIFACT_URL"
      exit 1
    fi
  else
    ASSET_NAME="pi-${VERSION}-${TARGET}${EXE_EXT}"
  fi

  SHA_URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/SHA256SUMS"
}

ensure_dest_dir() {
  mkdir -p "$DEST" 2>/dev/null || true
  if [ ! -d "$DEST" ]; then
    err "Install directory does not exist and could not be created: $DEST"
    exit 1
  fi
  if [ ! -w "$DEST" ]; then
    err "No write permission for install directory: $DEST"
    if [ "$SYSTEM" -eq 1 ]; then
      err "Re-run with sudo for --system installs"
    else
      err "Choose a writable directory with --dest"
    fi
    exit 1
  fi

  local resolved_dest=""
  if resolved_dest="$(cd "$DEST" 2>/dev/null && pwd -P)"; then
    DEST="$resolved_dest"
  fi
}

check_disk_space() {
  local min_kb=20480
  local path="$DEST"
  if [ ! -d "$path" ]; then
    path=$(dirname "$path")
  fi

  if command -v df >/dev/null 2>&1; then
    local avail_kb
    avail_kb=$(df -Pk "$path" 2>/dev/null | awk 'NR==2 {print $4}')
    if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "$min_kb" ]; then
      err "Insufficient disk space in $path (need at least 20MB free)"
      exit 1
    fi
  else
    warn "df not found; skipping disk space preflight check"
  fi
}

check_existing_install() {
  local existing="$DEST/pi"
  if [ -x "$existing" ]; then
    local current
    current="$(capture_version_line "$existing")"
    if [ -n "$current" ]; then
      info "Existing pi detected at $existing: $current"
    fi
  fi
}

check_network_preflight() {
  if [ "$OFFLINE" -eq 1 ]; then
    info "Offline mode enabled; skipping network preflight"
    return 0
  fi

  if ! command -v curl >/dev/null 2>&1; then
    warn "curl not found; skipping network preflight check"
    return 0
  fi

  local probe_url=""
  if [ "$FROM_SOURCE" -eq 1 ]; then
    probe_url="https://github.com/${OWNER}/${REPO}"
  elif [ -n "$ARTIFACT_URL" ]; then
    probe_url="$ARTIFACT_URL"
  else
    probe_url="$SHA_URL"
  fi

  if [ -z "$probe_url" ]; then
    return 0
  fi

  if ! probe_url_head "$probe_url" "network preflight probe"; then
    warn "Network preflight failed for $probe_url"
    warn "Continuing; install may still succeed on retry"
  fi
}

preflight_checks() {
  info "Running installer preflight checks"
  check_disk_space
  check_existing_install
  check_network_preflight
}

validate_options() {
  case "$RETAIN_TEMP" in
    0|1)
      ;;
    *)
      err "PI_INSTALLER_RETAIN_TEMP must be 0 or 1"
      exit 1
      ;;
  esac

  case "$LOCK_DIR" in
    /*)
      ;;
    *)
      err "PI_INSTALLER_LOCK_DIR must be an absolute path"
      exit 1
      ;;
  esac
  case "$LOCK_DIR" in
    /|*/|*//*|*/./*|*/.|*/../*|*/..|*$'\t'*|*$'\n'*|*$'\r'*)
      err "PI_INSTALLER_LOCK_DIR is unsafe"
      exit 1
      ;;
  esac

  case "$AGENT_SKILLS_ENABLED" in
    0|1)
      ;;
    *)
      warn "Invalid AGENT_SKILLS_ENABLED value '$AGENT_SKILLS_ENABLED'; defaulting to 1"
      AGENT_SKILLS_ENABLED=1
      ;;
  esac

  case "$COMPLETIONS_MODE" in
    auto|off|bash|zsh|fish)
      ;;
    *)
      err "Invalid --completions value '$COMPLETIONS_MODE' (expected auto|off|bash|zsh|fish)"
      exit 1
      ;;
  esac

  if [ "$FROM_SOURCE" -eq 1 ] && [ -n "$ARTIFACT_URL" ]; then
    warn "--artifact-url is ignored with --from-source"
  fi

  if [ "$NO_VERIFY" -eq 1 ] && { [ -n "$CHECKSUM" ] || [ -n "$CHECKSUM_URL" ] || [ -n "$SIGSTORE_BUNDLE_URL" ]; }; then
    warn "--no-verify set; checksum/signature override flags are ignored"
  fi

  if [ -n "$CHECKSUM" ] && [[ ! "$CHECKSUM" =~ ^[0-9a-fA-F]{64}$ ]]; then
    err "--checksum must be a 64-character hex SHA256 digest"
    exit 1
  fi

  if [ -n "$CHECKSUM" ] && [ -n "$CHECKSUM_URL" ]; then
    warn "Both --checksum and --checksum-url provided; --checksum takes precedence"
  fi

  if [ "$OFFLINE" -eq 1 ] && [ -n "$ARTIFACT_URL" ] && ! is_local_resource_ref "$ARTIFACT_URL"; then
    err "Offline mode requires a local --artifact-url path (or use --offline <tarball>)"
    exit 1
  fi

  if [ "$OFFLINE" -eq 1 ] && [ -n "$CHECKSUM_URL" ] && ! is_local_resource_ref "$CHECKSUM_URL"; then
    err "Offline mode requires a local --checksum-url path"
    exit 1
  fi

  if [ "$OFFLINE" -eq 1 ] && [ -n "$SIGSTORE_BUNDLE_URL" ] && ! is_local_resource_ref "$SIGSTORE_BUNDLE_URL"; then
    err "Offline mode requires a local --sigstore-bundle-url path"
    exit 1
  fi

  if [ "$OFFLINE" -eq 1 ] && [ -z "$OFFLINE_TARBALL" ] && [ -z "$ARTIFACT_URL" ] && [ "$FROM_SOURCE" -eq 0 ]; then
    err "Offline mode requires a local artifact path via --offline <tarball> or --artifact-url <local file>"
    exit 1
  fi

  if [ "$OFFLINE" -eq 1 ] && [ "$FROM_SOURCE" -eq 1 ] && [ -z "$SOURCE_DIR" ]; then
    err "--offline cannot be combined with --from-source unless --source-dir is also set (the implicit clone needs network access)"
    exit 1
  fi

  if [ -n "$OFFLINE_TARBALL" ]; then
    OFFLINE=1
    if [ -n "$ARTIFACT_URL" ]; then
      err "Pass either --offline <tarball> or --artifact-url, not both"
      exit 1
    fi
    if ! is_local_resource_ref "$OFFLINE_TARBALL"; then
      err "--offline expects a local artifact path (absolute, relative, or file://)"
      exit 1
    fi

    local offline_path
    offline_path="$(resource_to_local_path "$OFFLINE_TARBALL")"
    if [ ! -f "$offline_path" ]; then
      err "Offline artifact not found: $offline_path"
      exit 1
    fi

    if [ -d "$(dirname "$offline_path")" ]; then
      local resolved_dir
      resolved_dir="$(cd "$(dirname "$offline_path")" && pwd -P)"
      offline_path="${resolved_dir}/$(basename "$offline_path")"
    fi

    OFFLINE_TARBALL="$offline_path"
    ARTIFACT_URL="file://${OFFLINE_TARBALL}"
  fi
}

check_dependencies() {
  if [ "$FROM_SOURCE" -eq 0 ]; then
    local needs_curl=1
    if [ -n "$ARTIFACT_URL" ] && is_local_resource_ref "$ARTIFACT_URL" \
      && { [ -z "$CHECKSUM_URL" ] || is_local_resource_ref "$CHECKSUM_URL"; } \
      && { [ -z "$SIGSTORE_BUNDLE_URL" ] || is_local_resource_ref "$SIGSTORE_BUNDLE_URL"; }; then
      needs_curl=0
    fi

    if [ "$needs_curl" -eq 1 ] && ! command -v curl >/dev/null 2>&1; then
      err "curl is required for release downloads"
      exit 1
    fi
  fi

  if [ "$FROM_SOURCE" -eq 1 ]; then
    if ! command -v git >/dev/null 2>&1; then
      err "git is required for --from-source installs"
      exit 1
    fi
    if ! command -v cargo >/dev/null 2>&1; then
      err "cargo is required for --from-source installs"
      err "Install Rust nightly first: https://rustup.rs"
      exit 1
    fi
  fi
}

acquire_lock() {
  if mkdir "$LOCK_DIR" 2>/dev/null; then
    LOCKED=1
    echo $$ > "$LOCK_DIR/pid"
    return 0
  fi

  if [ -d "$LOCK_DIR" ] && [ ! -L "$LOCK_DIR" ] \
    && [ -f "$LOCK_DIR/pid" ] && [ ! -L "$LOCK_DIR/pid" ]; then
    local old_pid stale_lock_dir
    old_pid=$(cat "$LOCK_DIR/pid" 2>/dev/null || true)
    case "$old_pid" in
      ''|*[!0-9]*|0)
        ;;
      *)
        if ! kill -0 "$old_pid" 2>/dev/null \
          && command -v ps >/dev/null 2>&1 \
          && ! ps -p "$old_pid" >/dev/null 2>&1; then
          stale_lock_dir="${LOCK_DIR}.stale.$(date -u +%Y%m%dT%H%M%SZ).$$.${RANDOM}"
          if [ ! -e "$stale_lock_dir" ] && [ ! -L "$stale_lock_dir" ] \
            && mv "$LOCK_DIR" "$stale_lock_dir" 2>/dev/null \
            && mkdir "$LOCK_DIR" 2>/dev/null; then
            LOCKED=1
            echo $$ > "$LOCK_DIR/pid"
            warn "Preserved stale installer lock: $stale_lock_dir"
            return 0
          fi
        fi
        ;;
    esac
  fi

  err "Another installer appears to be running: $LOCK_DIR"
  exit 1
}

cleanup() {
  local exit_code=$?

  if [ "$exit_code" -ne 0 ] && [ "$MIGRATION_MOVED" -eq 1 ] && [ "$INSTALL_COMMITTED" -eq 0 ]; then
    if [ -n "$LEGACY_MOVED_FROM" ] && [ -n "$LEGACY_MOVED_TO" ] && [ -e "$LEGACY_MOVED_TO" ] && [ ! -e "$LEGACY_MOVED_FROM" ]; then
      mv "$LEGACY_MOVED_TO" "$LEGACY_MOVED_FROM" 2>/dev/null || true
      warn "Rolled back legacy pi preservation due to installer failure"
    fi
  fi

  if [ "$RETAIN_TEMP" -eq 1 ]; then
    if [ -n "$TMP" ] && [ -d "$TMP" ]; then
      warn "Retaining installer temporary directory: $TMP"
    fi
    if [ "$LOCKED" -eq 1 ] && [ -d "$LOCK_DIR" ]; then
      warn "Retaining installer lock directory: $LOCK_DIR"
    fi
  else
    if [ -n "$TMP" ] && [ -d "$TMP" ]; then
      remove_path_recursively "$TMP" 2>/dev/null || true
    fi
    if [ "$LOCKED" -eq 1 ]; then
      rm -f "$LOCK_DIR/pid" 2>/dev/null || true
      rmdir "$LOCK_DIR" 2>/dev/null || true
    fi
  fi

  trap - EXIT
  exit "$exit_code"
}

trap cleanup EXIT

is_rust_pi_output() {
  local out="$1"
  [[ "$out" =~ ^pi[[:space:]][0-9]+\.[0-9]+\.[0-9]+[[:space:]]\( ]]
}

looks_like_node_script() {
  local path="$1"
  [ -f "$path" ] || return 1

  if [[ "$path" == *.js ]] || [[ "$path" == *node_modules* ]]; then
    return 0
  fi

  local head_line
  head_line=$(head -n 1 "$path" 2>/dev/null || true)
  if [[ "$head_line" == *node* ]]; then
    return 0
  fi

  return 1
}

detect_existing_pi() {
  CURRENT_PI_PATH=$(command -v pi 2>/dev/null || true)
  CURRENT_PI_VERSION=""
  TS_PI_DETECTED=0

  if [ -z "$CURRENT_PI_PATH" ]; then
    return 0
  fi

  CURRENT_PI_VERSION="$(capture_version_line "$CURRENT_PI_PATH")"

  if is_rust_pi_output "$CURRENT_PI_VERSION"; then
    TS_PI_DETECTED=0
    return 0
  fi

  if looks_like_node_script "$CURRENT_PI_PATH"; then
    TS_PI_DETECTED=1
    return 0
  fi

  if command -v npm >/dev/null 2>&1; then
    if npm list -g --depth=0 @mariozechner/pi-coding-agent >/dev/null 2>&1; then
      TS_PI_DETECTED=1
      return 0
    fi
  fi

  if [ -n "$CURRENT_PI_VERSION" ] && ! is_rust_pi_output "$CURRENT_PI_VERSION"; then
    TS_PI_DETECTED=1
  fi
}

choose_adoption_mode() {
  ADOPT_TS=0
  ADOPT_CANONICAL=0
  FINAL_BIN_NAME="pi"

  if [ "$TS_PI_DETECTED" -eq 0 ]; then
    return 0
  fi

  info "Detected existing non-Rust pi command at: $CURRENT_PI_PATH"
  if [ -n "$CURRENT_PI_VERSION" ]; then
    info "Existing pi reports: $CURRENT_PI_VERSION"
  fi

  local decision=""
  case "$ADOPT_MODE" in
    yes)
      decision="yes"
      ;;
    no)
      decision="no"
      ;;
    ask)
      if prompt_confirm "Install Rust Pi as canonical 'pi' and preserve existing one as '${LEGACY_ALIAS_NAME}'?" 0; then
        decision="yes"
      else
        decision="no"
      fi
      ;;
    *)
      decision="no"
      ;;
  esac

  if [ "$decision" = "yes" ]; then
    ADOPT_TS=1
    ADOPT_CANONICAL=1
  else
    ADOPT_TS=0
    ADOPT_CANONICAL=0
    FINAL_BIN_NAME="pi-rust"
    warn "Keeping existing pi untouched; Rust binary will be installed as ${FINAL_BIN_NAME}"
  fi
}

choose_dest_for_adoption() {
  if [ "$ADOPT_TS" -ne 1 ]; then
    return 0
  fi

  local current_dir=""
  if [ -n "$CURRENT_PI_PATH" ]; then
    current_dir=$(dirname "$CURRENT_PI_PATH")
  fi

  if [ "$DEST_EXPLICIT" -eq 1 ]; then
    if [ -n "$current_dir" ] && [ "$DEST" = "$current_dir" ]; then
      ADOPT_CANONICAL=1
    else
      ADOPT_CANONICAL=0
    fi
    return 0
  fi

  if [ -z "$CURRENT_PI_PATH" ]; then
    return 0
  fi

  if [ -w "$current_dir" ]; then
    DEST="$current_dir"
    ADOPT_CANONICAL=1
    info "Using existing pi directory for canonical replacement: $DEST"
    return 0
  fi

  ADOPT_CANONICAL=0
  warn "Cannot write to existing pi directory: $current_dir"
  warn "Will install to default destination: $DEST"
  warn "Enable --easy-mode to prepend that path for future shells"
}

ensure_install_target() {
  INSTALL_BIN_PATH="$DEST/$FINAL_BIN_NAME"
}

compute_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
    return 0
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
    return 0
  fi
  err "No SHA256 tool found (sha256sum or shasum)"
  return 1
}

verify_download_checksum() {
  local artifact_file="$1"
  local asset_name="${2:-$ASSET_NAME}"
  local artifact_url="${3:-}"

  if [ "$NO_VERIFY" -eq 1 ]; then
    CHECKSUM_STATUS="skipped (--no-verify)"
    return 0
  fi

  local checksum_file="$TMP/checksum.txt"
  local expected=""
  local checksum_source_kind="release-manifest"

  if [ -n "$CHECKSUM" ]; then
    expected="$CHECKSUM"
    checksum_source_kind="inline"
  elif [ -n "$CHECKSUM_URL" ]; then
    if ! fetch_url_to_file "$CHECKSUM_URL" "$checksum_file" "checksum file"; then
      err "Failed to download checksum file: $CHECKSUM_URL"
      return 4
    fi
    checksum_source_kind="custom-url"
  elif [ -n "$ARTIFACT_URL" ]; then
    local artifact_base="${artifact_url%%\?*}"
    artifact_base="${artifact_base%%#*}"
    local derived_checksum_url="${artifact_base}.sha256"
    if ! fetch_url_to_file "$derived_checksum_url" "$checksum_file" "derived checksum file"; then
      err "Failed to download checksum file: $derived_checksum_url"
      err "Provide --checksum or --checksum-url for custom artifact installs"
      return 4
    fi
    checksum_source_kind="artifact-derived"
  else
    if ! fetch_url_to_file "$SHA_URL" "$checksum_file" "release checksum manifest"; then
      warn "No SHA256SUMS found in release; skipping checksum verification"
      CHECKSUM_STATUS="skipped (no SHA256SUMS in release)"
      return 0
    fi
  fi

  if [ -z "$expected" ]; then
    expected=$(awk -v name="$asset_name" '
      $1 ~ /^[0-9a-fA-F]{64}$/ {
        file=$2
        sub(/^\*/, "", file)
        sub(/^\.\//, "", file)
        if (file == name) {
          print $1
          exit
        }
      }
    ' "$checksum_file")

    if [ -z "$expected" ]; then
      if [ "$checksum_source_kind" != "release-manifest" ]; then
        local checksum_count
        checksum_count=$(awk '$1 ~ /^[0-9a-fA-F]{64}$/ { c += 1 } END { print c + 0 }' "$checksum_file")
        if [ "$checksum_count" -eq 1 ]; then
          expected=$(awk '$1 ~ /^[0-9a-fA-F]{64}$/ { print $1; exit }' "$checksum_file")
        fi
      fi
    fi

    if [ -z "$expected" ]; then
      CHECKSUM_STATUS="failed (missing checksum entry)"
      return 2
    fi
  fi

  local actual
  actual=$(compute_sha256 "$artifact_file")
  if [ "$actual" != "$expected" ]; then
    CHECKSUM_STATUS="failed (mismatch)"
    err "Checksum mismatch for $asset_name"
    err "Expected: $expected"
    err "Actual:   $actual"
    return 3
  fi

  local source_desc="SHA256SUMS"
  if [ -n "$CHECKSUM" ]; then
    source_desc="--checksum"
  elif [ -n "$CHECKSUM_URL" ]; then
    source_desc="--checksum-url"
  elif [ -n "$ARTIFACT_URL" ]; then
    source_desc="artifact .sha256"
  fi

  CHECKSUM_STATUS="verified (${source_desc})"
  ok "Checksum verified for ${asset_name}"
  if [ -n "$artifact_url" ] && [ "$QUIET" -eq 0 ]; then
    log "  checksum source: $source_desc"
  fi
}

verify_sigstore_bundle() {
  local artifact_file="$1"
  local artifact_url="$2"

  if [ "$NO_VERIFY" -eq 1 ]; then
    SIGSTORE_STATUS="skipped (--no-verify)"
    return 0
  fi

  if ! command -v cosign >/dev/null 2>&1; then
    SIGSTORE_STATUS="skipped (cosign not found)"
    warn "cosign not found; skipping signature verification"
    return 0
  fi

  local bundle_url="$SIGSTORE_BUNDLE_URL"
  if [ "$OFFLINE" -eq 1 ] && [ -z "$bundle_url" ]; then
    SIGSTORE_STATUS="skipped (offline; bundle not provided)"
    warn "Offline mode: skipping signature verification without --sigstore-bundle-url"
    return 0
  fi
  if [ -z "$bundle_url" ]; then
    local artifact_base="${artifact_url%%\?*}"
    artifact_base="${artifact_base%%#*}"
    bundle_url="${artifact_base}.sigstore.json"
  fi

  local bundle_file
  bundle_file="$TMP/$(basename "${bundle_url%%\?*}")"
  if ! fetch_url_to_file "$bundle_url" "$bundle_file" "sigstore bundle"; then
    SIGSTORE_STATUS="skipped (bundle unavailable)"
    warn "Sigstore bundle not found; skipping signature verification"
    return 0
  fi

  if ! cosign verify-blob \
    --bundle "$bundle_file" \
    --certificate-identity-regexp "$COSIGN_IDENTITY_RE" \
    --certificate-oidc-issuer "$COSIGN_OIDC_ISSUER" \
    "$artifact_file"; then
    SIGSTORE_STATUS="failed"
    err "Sigstore verification failed for $(basename "$artifact_file")"
    return 1
  fi

  SIGSTORE_STATUS="verified"
  ok "Signature verified (cosign)"
}

extract_release_artifact() {
  local candidate="$1"
  local artifact_file="$2"

  if [[ "$candidate" == *.tar.xz ]]; then
    if ! command -v tar >/dev/null 2>&1; then
      warn "tar is not available to extract $candidate"
      return 1
    fi
    if ! command -v xz >/dev/null 2>&1; then
      warn "xz is not available to extract $candidate"
      return 1
    fi
    local extract_dir="$TMP/extract-${candidate//\//_}"
    mkdir -p "$extract_dir"
    if ! tar -xJf "$artifact_file" -C "$extract_dir"; then
      warn "Failed to extract archive: $candidate"
      return 1
    fi
    local found_bin=""
    found_bin="$(find "$extract_dir" -type f \( -name "pi${EXE_EXT}" -o -name "pi" -o -name "pi.exe" \) | head -1)"
    if [ -z "$found_bin" ]; then
      warn "archive '$candidate' did not contain a pi binary"
      return 1
    fi
    chmod +x "$found_bin" 2>/dev/null || true
    printf '%s\n' "$found_bin"
    return 0
  fi

  if [[ "$candidate" == *.tar.gz ]] || [[ "$candidate" == *.tgz ]]; then
    if ! command -v tar >/dev/null 2>&1; then
      warn "tar is not available to extract $candidate"
      return 1
    fi
    local extract_dir="$TMP/extract-${candidate//\//_}"
    mkdir -p "$extract_dir"
    if ! tar -xzf "$artifact_file" -C "$extract_dir"; then
      warn "Failed to extract archive: $candidate"
      return 1
    fi
    local found_bin=""
    found_bin="$(find "$extract_dir" -type f \( -name "pi${EXE_EXT}" -o -name "pi" -o -name "pi.exe" \) | head -1)"
    if [ -z "$found_bin" ]; then
      warn "archive '$candidate' did not contain a pi binary"
      return 1
    fi
    chmod +x "$found_bin" 2>/dev/null || true
    printf '%s\n' "$found_bin"
    return 0
  fi

  if [[ "$candidate" == *.zip ]]; then
    if ! command -v unzip >/dev/null 2>&1; then
      warn "unzip is not available to extract $candidate"
      return 1
    fi
    local extract_dir="$TMP/extract-${candidate//\//_}"
    mkdir -p "$extract_dir"
    if ! unzip -q "$artifact_file" -d "$extract_dir"; then
      warn "Failed to extract archive: $candidate"
      return 1
    fi
    local found_bin=""
    found_bin="$(find "$extract_dir" -type f \( -name "pi${EXE_EXT}" -o -name "pi" -o -name "pi.exe" \) | head -1)"
    if [ -z "$found_bin" ]; then
      warn "archive '$candidate' did not contain a pi binary"
      return 1
    fi
    chmod +x "$found_bin" 2>/dev/null || true
    printf '%s\n' "$found_bin"
    return 0
  fi

  chmod +x "$artifact_file" 2>/dev/null || true
  printf '%s\n' "$artifact_file"
}

download_release_binary() {
  local candidates=()
  if [ -n "$ARTIFACT_URL" ]; then
    candidates+=("$ASSET_NAME|$ARTIFACT_URL")
  else
    # Try candidates in priority order. dsr bare-binary names first (most common
    # for local releases), then archive formats, then Rust target-triple names.
    local base_v="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}"
    # dsr-style naming: pi_<os>_<arch> with underscores (e.g. pi_darwin_arm64)
    if [ -n "$ASSET_PLATFORM" ]; then
      local dsr_name="pi_${ASSET_PLATFORM//-/_}${EXE_EXT}"
      candidates+=("${dsr_name}|${base_v}/${dsr_name}")
    fi
    # Bare binary name (dsr uploads Linux as just "pi")
    candidates+=("pi${EXE_EXT}|${base_v}/pi${EXE_EXT}")
    # Archive formats (GH Actions output)
    if [ -n "$ASSET_PLATFORM" ]; then
      if [ -n "$EXE_EXT" ]; then
        candidates+=("pi-${ASSET_PLATFORM}.zip|${base_v}/pi-${ASSET_PLATFORM}.zip")
      else
        candidates+=("pi-${ASSET_PLATFORM}.tar.xz|${base_v}/pi-${ASSET_PLATFORM}.tar.xz")
        candidates+=("pi-${ASSET_PLATFORM}.tar.gz|${base_v}/pi-${ASSET_PLATFORM}.tar.gz")
      fi
    fi
    # Rust target-triple naming
    candidates+=("pi-${TARGET}${EXE_EXT}|${base_v}/pi-${TARGET}${EXE_EXT}")
    candidates+=("pi-${OS}-${ARCH}${EXE_EXT}|${base_v}/pi-${OS}-${ARCH}${EXE_EXT}")
  fi

  local entry=""
  for entry in "${candidates[@]}"; do
    local candidate="${entry%%|*}"
    local candidate_url="${entry#*|}"
    local artifact_file="$TMP/$candidate"
    # Suppress stderr for candidate probing — 404s are expected as we try
    # multiple naming conventions. Only show errors for explicit --artifact-url.
    if ! fetch_url_to_file "$candidate_url" "$artifact_file" "release artifact" 2>/dev/null; then
      if [ -n "$ARTIFACT_URL" ]; then
        err "Failed to download artifact: $candidate_url"
      fi
      continue
    fi

    ASSET_NAME="$candidate"

    local checksum_rc=0
    if verify_download_checksum "$artifact_file" "$candidate" "$candidate_url"; then
      :
    else
      checksum_rc=$?
      if [ "$checksum_rc" -eq 2 ]; then
        if [ -n "$ARTIFACT_URL" ] || [ -n "$CHECKSUM" ] || [ -n "$CHECKSUM_URL" ]; then
          err "No checksum entry found for $candidate"
          return "$checksum_rc"
        fi
        warn "No checksum entry for $candidate in SHA256SUMS; trying next candidate"
        continue
      fi
      return "$checksum_rc"
    fi

    if ! verify_sigstore_bundle "$artifact_file" "$candidate_url"; then
      return 5
    fi

    local extracted=""
    extracted="$(extract_release_artifact "$candidate" "$artifact_file" || true)"
    if [ -n "$extracted" ] && [ -e "$extracted" ]; then
      printf '%s\n' "$extracted"
      return 0
    fi
  done

  err "No downloadable release artifact found for version ${VERSION} and target ${TARGET}"
  return 1
}

build_from_source() {
  local src_dir

  if [ -n "$SOURCE_DIR" ]; then
    # Local-source build path: skip the clone entirely. Useful for FreeBSD
    # (no prebuilt binaries), feature-branch iteration where the desired
    # commit is not tagged, and air-gapped/offline workflows that already
    # have the source on disk. Validate the directory looks like a real
    # pi_agent_rust checkout before invoking cargo.
    if [ ! -d "$SOURCE_DIR" ]; then
      err "--source-dir path does not exist or is not a directory: $SOURCE_DIR"
      return 1
    fi
    src_dir="$(cd "$SOURCE_DIR" && pwd -P)" || {
      err "Could not resolve --source-dir to an absolute path: $SOURCE_DIR"
      return 1
    }
    if [ ! -f "$src_dir/Cargo.toml" ]; then
      err "--source-dir does not contain Cargo.toml: $src_dir"
      return 1
    fi
    if ! grep -q '^name *= *"pi_agent_rust"' "$src_dir/Cargo.toml"; then
      err "--source-dir Cargo.toml package name is not pi_agent_rust: $src_dir"
      return 1
    fi
  else
    if [ "$OFFLINE" -eq 1 ]; then
      err "Offline mode cannot build from source without --source-dir (the implicit clone needs network)"
      return 1
    fi
    src_dir="$TMP/src"
    git clone --depth 1 --branch "$VERSION" "https://github.com/${OWNER}/${REPO}.git" "$src_dir" >&2
  fi

  (cd "$src_dir" && cargo build --release --locked --bin pi >&2)

  local built_bin="$src_dir/target/release/pi${EXE_EXT}"
  if [ ! -x "$built_bin" ]; then
    err "Source build succeeded but binary was not found: $built_bin"
    return 1
  fi

  printf '%s\n' "$built_bin"
}

install_binary_file() {
  local source_bin="$1"
  install -m 0755 "$source_bin" "$INSTALL_BIN_PATH"
  ok "Installed $FINAL_BIN_NAME to $INSTALL_BIN_PATH"
}

create_managed_alias_wrapper() {
  local alias_path="$1"
  local target_path="$2"
  local label="$3"

  if [ -z "$alias_path" ] || [ -z "$target_path" ] || [ -z "$label" ]; then
    return 1
  fi

  {
    printf '#!/usr/bin/env bash\n'
    printf '# pi_agent_rust installer managed alias\n'
    printf 'set -euo pipefail\n'
    printf 'exec %q "$@"\n' "$target_path"
  } > "$alias_path"
  chmod 0755 "$alias_path"
  ok "Created ${label} alias: $alias_path"
}

choose_legacy_alias_path() {
  local candidate="$DEST/$LEGACY_ALIAS_NAME"
  if [ ! -e "$candidate" ]; then
    LEGACY_ALIAS_PATH="$candidate"
    return 0
  fi

  if grep -q "pi_agent_rust installer managed alias" "$candidate" 2>/dev/null; then
    LEGACY_ALIAS_PATH="$candidate"
    return 0
  fi

  local alt="$DEST/${LEGACY_ALIAS_NAME}-ts"
  if [ ! -e "$alt" ]; then
    warn "Existing $candidate is not installer-managed; using ${LEGACY_ALIAS_NAME}-ts instead"
    LEGACY_ALIAS_PATH="$alt"
    return 0
  fi

  local idx=1
  while :; do
    alt="$DEST/${LEGACY_ALIAS_NAME}-ts-${idx}"
    if [ ! -e "$alt" ]; then
      warn "Using alternate legacy alias: $(basename "$alt")"
      LEGACY_ALIAS_PATH="$alt"
      return 0
    fi
    idx=$((idx + 1))
  done
}

create_legacy_alias_wrapper() {
  local alias_path="$1"
  local target_path="$2"

  if [ -z "$alias_path" ] || [ -z "$target_path" ]; then
    return 1
  fi

  create_managed_alias_wrapper "$alias_path" "$target_path" "legacy"
}

prepare_typescript_migration() {
  MIGRATION_MOVED=0
  LEGACY_ALIAS_PATH=""
  LEGACY_TARGET_PATH=""
  LEGACY_MOVED_FROM=""
  LEGACY_MOVED_TO=""

  if [ "$ADOPT_TS" -ne 1 ]; then
    return 0
  fi

  choose_legacy_alias_path

  if [ -z "$CURRENT_PI_PATH" ]; then
    warn "No existing pi command path found; skipping legacy alias creation"
    return 0
  fi

  local current_real="$CURRENT_PI_PATH"
  if [ "$current_real" = "$INSTALL_BIN_PATH" ] && [ -e "$current_real" ]; then
    local preserve_candidate="$DEST/.pi-legacy-typescript"
    if [ -e "$preserve_candidate" ]; then
      local stamp
      stamp=$(date +%Y%m%d%H%M%S)
      preserve_candidate="$DEST/.pi-legacy-typescript.${stamp}"
    fi

    mv "$current_real" "$preserve_candidate"
    LEGACY_MOVED_FROM="$current_real"
    LEGACY_MOVED_TO="$preserve_candidate"
    LEGACY_TARGET_PATH="$preserve_candidate"
    MIGRATION_MOVED=1
    ok "Preserved existing pi binary at: $preserve_candidate"
  else
    LEGACY_TARGET_PATH="$current_real"
    info "Existing pi remains at: $current_real"
  fi

  create_legacy_alias_wrapper "$LEGACY_ALIAS_PATH" "$LEGACY_TARGET_PATH"
}

ensure_compat_alias() {
  COMPAT_ALIAS_PATH=""

  if [ -n "$EXE_EXT" ]; then
    COMPAT_ALIAS_STATUS="skipped (managed ${COMPAT_ALIAS_NAME} alias is unix-only)"
    return 0
  fi

  if [ -z "$COMPAT_ALIAS_NAME" ] || [ "$COMPAT_ALIAS_NAME" = "$FINAL_BIN_NAME" ]; then
    COMPAT_ALIAS_STATUS="skipped (no secondary alias requested)"
    return 0
  fi

  local alias_path="$DEST/$COMPAT_ALIAS_NAME"
  if [ -e "$alias_path" ] && ! is_managed_alias "$alias_path"; then
    COMPAT_ALIAS_STATUS="skipped (existing non-managed ${COMPAT_ALIAS_NAME} at ${alias_path})"
    warn "Existing ${COMPAT_ALIAS_NAME} at ${alias_path} is not installer-managed; leaving it untouched"
    return 0
  fi

  if [ ! -e "$alias_path" ]; then
    local existing_command=""
    existing_command=$(command -v "$COMPAT_ALIAS_NAME" 2>/dev/null || true)
    if [ -n "$existing_command" ] && [ "$existing_command" != "$alias_path" ]; then
      COMPAT_ALIAS_STATUS="skipped (existing ${COMPAT_ALIAS_NAME} command at ${existing_command})"
      warn "Existing ${COMPAT_ALIAS_NAME} command detected at ${existing_command}; skipping managed alias"
      return 0
    fi
  fi

  create_managed_alias_wrapper "$alias_path" "$INSTALL_BIN_PATH" "$COMPAT_ALIAS_NAME"
  COMPAT_ALIAS_PATH="$alias_path"
  COMPAT_ALIAS_STATUS="installed (${COMPAT_ALIAS_NAME} -> $INSTALL_BIN_PATH)"
}

maybe_add_path() {
  case ":$PATH:" in
    *":$DEST:"*)
      return 0
      ;;
  esac

  if [ "$EASY" -ne 1 ]; then
    warn "Add this directory to PATH to use installed binaries: $DEST"
    return 0
  fi

  local updated=""
  for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
    if [ -e "$rc" ] && [ ! -w "$rc" ]; then
      continue
    fi

    if [ ! -e "$rc" ]; then
      : > "$rc"
    fi

    if grep -F "$PATH_MARKER" "$rc" >/dev/null 2>&1; then
      continue
    fi

    printf "\nexport PATH=\"%s:\$PATH\" %s\n" "$DEST" "$PATH_MARKER" >> "$rc"
    if [ -z "$updated" ]; then
      updated="$rc"
    else
      updated="$updated:$rc"
    fi
  done

  PATH_UPDATED_FILES="$updated"

  if [ -n "$updated" ]; then
    ok "Updated PATH in shell rc files"
    warn "Restart your shell (or source rc files) to use updated PATH"
  else
    warn "Could not update PATH automatically; add $DEST manually"
  fi
}

detect_default_shell() {
  local shell_name="${SHELL:-}"
  [ -z "$shell_name" ] && return 1
  shell_name=$(basename "$shell_name")
  case "$shell_name" in
    bash|zsh|fish)
      printf '%s\n' "$shell_name"
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

install_completions_for_shell() {
  local shell_name="$1"
  local bin="$INSTALL_BIN_PATH"

  if [ ! -x "$bin" ]; then
    COMPLETIONS_STATUS="skipped (binary missing)"
    return 1
  fi

  local subcommand=""
  local probe_timeout="${PI_INSTALLER_COMPLETION_PROBE_TIMEOUT:-3}"
  local generation_timeout="${PI_INSTALLER_COMPLETION_CMD_TIMEOUT:-10}"

  # Prefer static command discovery from top-level --help (safe, fast path).
  # If that fails, fall back to legacy subcommand probes guarded by a timeout.
  local root_help=""
  local root_help_ok=0
  local root_help_rc=0
  local should_probe_subcommands=0
  if root_help="$(run_command_with_timeout_capture "$probe_timeout" "$bin" --help)"; then
    root_help_ok=1
  else
    root_help_rc=$?
  fi

  if [ "$root_help_ok" -eq 1 ]; then
    if printf '%s\n' "$root_help" | grep -Eq '^[[:space:]]+completions([[:space:]]|$)'; then
      subcommand="completions"
    elif printf '%s\n' "$root_help" | grep -Eq '^[[:space:]]+completion([[:space:]]|$)'; then
      subcommand="completion"
    elif printf '%s\n' "$root_help" | grep -Eq '^[[:space:]]*Commands:'; then
      # Help output is conclusive and lists no completion command.
      should_probe_subcommands=0
    else
      # Help output was available but inconclusive for command discovery.
      should_probe_subcommands=1
    fi
  else
    should_probe_subcommands=1
  fi

  if [ "$should_probe_subcommands" -eq 1 ]; then
    # Help probe was unavailable or inconclusive: guard runtime probes with timeout.
    local probe_rc=0
    local probe_timed_out=0
    if run_command_with_timeout_capture "$probe_timeout" "$bin" completions --help >/dev/null 2>&1; then
      subcommand="completions"
    else
      probe_rc=$?
      if [ "$probe_rc" -eq 124 ] || [ "$probe_rc" -eq 137 ]; then
        probe_timed_out=1
      fi
      if run_command_with_timeout_capture "$probe_timeout" "$bin" completion --help >/dev/null 2>&1; then
        subcommand="completion"
      else
        probe_rc=$?
        if [ "$probe_rc" -eq 124 ] || [ "$probe_rc" -eq 137 ]; then
          probe_timed_out=1
        fi
      fi
    fi

    if [ -z "$subcommand" ] && [ "$probe_timed_out" -eq 1 ]; then
      COMPLETIONS_STATUS="failed (completion probe timed out)"
      warn "Shell completions probe timed out; skipping completion installation"
      return 1
    fi
  fi

  if [ -z "$subcommand" ] && [ "$root_help_ok" -eq 0 ] && [ "$root_help_rc" -eq 125 ]; then
    COMPLETIONS_STATUS="skipped (completion probe unavailable)"
    info "Shell completions: skipped (unable to safely probe completion support)"
    return 0
  fi

  if [ -z "$subcommand" ]; then
    COMPLETIONS_STATUS="skipped (unsupported by this pi build)"
    info "Shell completions: skipped (binary has no completion subcommand)"
    return 0
  fi

  local target=""
  case "$shell_name" in
    bash)
      target="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions/${FINAL_BIN_NAME}"
      ;;
    zsh)
      target="${XDG_DATA_HOME:-$HOME/.local/share}/zsh/site-functions/_${FINAL_BIN_NAME}"
      ;;
    fish)
      target="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions/${FINAL_BIN_NAME}.fish"
      ;;
    *)
      COMPLETIONS_STATUS="skipped (unsupported shell: $shell_name)"
      return 1
      ;;
  esac

  if ! mkdir -p "$(dirname "$target")" 2>/dev/null; then
    COMPLETIONS_STATUS="failed (cannot create completion dir)"
    warn "Failed to create shell completion directory for $shell_name"
    return 1
  fi

  local completion_output
  local completion_rc=0
  completion_output="$(run_command_with_timeout_capture "$generation_timeout" "$bin" "$subcommand" "$shell_name")" || completion_rc=$?
  if [ "$completion_rc" -ne 0 ]; then
    if [ "$completion_rc" -eq 124 ] || [ "$completion_rc" -eq 137 ]; then
      COMPLETIONS_STATUS="failed (completion generation timed out)"
      warn "Failed to generate $shell_name completions (timed out)"
      return 1
    fi
    COMPLETIONS_STATUS="failed (completion generation error)"
    warn "Failed to generate $shell_name completions"
    return 1
  fi
  if [ -z "$completion_output" ]; then
    COMPLETIONS_STATUS="failed (completion generation error)"
    warn "Failed to generate $shell_name completions"
    return 1
  fi

  if ! printf '%s\n' "$completion_output" > "$target"; then
    COMPLETIONS_STATUS="failed (write error)"
    warn "Failed to write $shell_name completions to $target"
    return 1
  fi
  ok "Installed $shell_name completions to $target"
  COMPLETIONS_STATUS="installed ($shell_name)"
  return 0
}

maybe_install_completions() {
  if [ "$COMPLETIONS_MODE" = "off" ]; then
    COMPLETIONS_STATUS="skipped (--no-completions)"
    return 0
  fi

  local shell_name="$COMPLETIONS_MODE"
  if [ "$shell_name" = "auto" ]; then
    if ! shell_name=$(detect_default_shell); then
      COMPLETIONS_STATUS="skipped (unknown shell)"
      info "Shell completions: skipped (unable to detect shell)"
      return 0
    fi
  fi

  install_completions_for_shell "$shell_name" || true
}

is_expected_legacy_agent_settings_path() {
  local path="$1"
  local agent="$2"
  [ -n "$path" ] || return 1

  case "$agent" in
    claude)
      case "$path" in
        "$HOME/.claude/settings.json"|"$HOME/.config/claude/settings.json"|"$HOME/Library/Application Support/Claude/settings.json")
          return 0
          ;;
      esac
      ;;
    gemini)
      case "$path" in
        "$HOME/.gemini/settings.json"|"$HOME/.gemini-cli/settings.json")
          return 0
          ;;
      esac
      ;;
  esac

  return 1
}

cleanup_legacy_settings_entries() {
  local settings_file="$1"
  local hook_key="$2"
  local matcher="$3"
  local require_name="$4"
  shift 4
  local bin_candidates=("$@")

  [ -f "$settings_file" ] || return 0
  [ "${#bin_candidates[@]}" -gt 0 ] || return 0
  command -v python3 >/dev/null 2>&1 || return 0

  local py_result=""
  if ! py_result=$(python3 - "$settings_file" "$hook_key" "$matcher" "$require_name" "${bin_candidates[@]}" <<'PYEOF'
import json
import os
import shlex
import sys

settings_file = sys.argv[1]
hook_key = sys.argv[2]
matcher = sys.argv[3]
require_name = sys.argv[4]
candidate_bins = [arg for arg in sys.argv[5:] if arg]

if not candidate_bins:
    print("NO_CANDIDATES")
    raise SystemExit(0)


def command_matches(command: str) -> bool:
    if not isinstance(command, str):
        return False
    cmd = command.strip()
    if not cmd:
        return False
    try:
        parts = shlex.split(cmd)
    except Exception:
        parts = cmd.split()
    if len(parts) != 1:
        return False
    first = parts[0]
    if not os.path.isabs(first):
        return False
    for bin_path in candidate_bins:
        if first == bin_path:
            return True
        try:
            if os.path.realpath(first) == os.path.realpath(bin_path):
                return True
        except Exception:
            pass
    return False


try:
    with open(settings_file, "r", encoding="utf-8") as f:
        settings = json.load(f)
except Exception:
    print("SKIP_INVALID_JSON")
    raise SystemExit(0)

if not isinstance(settings, dict):
    print("SKIP_INVALID_JSON")
    raise SystemExit(0)

hooks = settings.get("hooks")
if not isinstance(hooks, dict):
    print("NO_HOOKS")
    raise SystemExit(0)

entries = hooks.get(hook_key)
if not isinstance(entries, list):
    print("NO_HOOKS")
    raise SystemExit(0)

removed = 0
changed = False
new_entries = []

for entry in entries:
    if isinstance(entry, dict) and entry.get("matcher") == matcher:
        existing_hooks = entry.get("hooks", [])
        if not isinstance(existing_hooks, list):
            existing_hooks = []

        kept = []
        for hook in existing_hooks:
            should_remove = False
            if isinstance(hook, dict):
                command = str(hook.get("command", ""))
                if require_name:
                    if (
                        str(hook.get("name", "")) == require_name
                        and str(hook.get("type", "")) == "command"
                        and (set(hook.keys()) <= {"name", "type", "command", "timeout"})
                        and hook.get("timeout", 5000) in (5000, "5000")
                        and command_matches(command)
                    ):
                        should_remove = True
                else:
                    if (
                        str(hook.get("type", "")) == "command"
                        and (set(hook.keys()) <= {"type", "command"})
                        and command_matches(command)
                    ):
                        should_remove = True

            if should_remove:
                removed += 1
                changed = True
                continue

            kept.append(hook)

        if kept:
            entry["hooks"] = kept
            new_entries.append(entry)
        elif existing_hooks:
            changed = True
    else:
        new_entries.append(entry)

if not changed:
    print("ALREADY_ABSENT")
    raise SystemExit(0)

hooks[hook_key] = new_entries
if not hooks[hook_key]:
    del hooks[hook_key]
if not hooks:
    settings.pop("hooks", None)

with open(settings_file, "w", encoding="utf-8") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")

print(f"REMOVED:{removed}")
PYEOF
  ); then
    warn "Legacy settings cleanup failed for $settings_file"
    return 0
  fi

  case "$py_result" in
    REMOVED:*)
      local count="${py_result#REMOVED:}"
      if [ "$count" -gt 0 ] 2>/dev/null; then
        ok "Removed ${count} legacy installer entries from $settings_file"
      fi
      ;;
  esac
}

cleanup_legacy_agent_settings() {
  local bin_candidates=()
  local recorded_bin="${PIAR_INSTALL_BIN:-}"
  local current_bin="${INSTALL_BIN_PATH:-}"

  if [ -n "$recorded_bin" ]; then
    bin_candidates+=("$recorded_bin")
  fi
  if [ -n "$current_bin" ] && [ "$current_bin" != "$recorded_bin" ]; then
    bin_candidates+=("$current_bin")
  fi
  [ "${#bin_candidates[@]}" -gt 0 ] || return 0

  local claude_candidates=()
  if [ -n "${PIAR_CLAUDE_HOOK_SETTINGS:-}" ]; then
    claude_candidates+=("${PIAR_CLAUDE_HOOK_SETTINGS}")
  fi
  claude_candidates+=(
    "$HOME/.claude/settings.json"
    "$HOME/.config/claude/settings.json"
    "$HOME/Library/Application Support/Claude/settings.json"
  )

  local gemini_candidates=()
  if [ -n "${PIAR_GEMINI_HOOK_SETTINGS:-}" ]; then
    gemini_candidates+=("${PIAR_GEMINI_HOOK_SETTINGS}")
  fi
  gemini_candidates+=(
    "$HOME/.gemini/settings.json"
    "$HOME/.gemini-cli/settings.json"
  )

  local settings_path=""
  for settings_path in "${claude_candidates[@]}"; do
    if is_expected_legacy_agent_settings_path "$settings_path" "claude"; then
      cleanup_legacy_settings_entries "$settings_path" "PreToolUse" "Bash" "" "${bin_candidates[@]}"
    fi
  done
  for settings_path in "${gemini_candidates[@]}"; do
    if is_expected_legacy_agent_settings_path "$settings_path" "gemini"; then
      cleanup_legacy_settings_entries "$settings_path" "BeforeTool" "run_shell_command" "pi-agent-rust" "${bin_candidates[@]}"
    fi
  done
}

is_installer_managed_skill_file() {
  local file="$1"
  [ -f "$file" ] || return 1
  grep -Fq "$AGENT_SKILL_MARKER" "$file" 2>/dev/null
}

is_expected_skill_destination() {
  local destination="$1"
  [ -n "$destination" ] || return 1
  case "$destination" in
    */skills/${AGENT_SKILL_NAME}) return 0 ;;
    *) return 1 ;;
  esac
}

pi_agent_skill_inline_content() {
  cat <<'SKILL'
---
name: pi-agent-rust
description: >-
  Speeds up pi_agent_rust development and verification workflows. Use when editing providers,
  tools, sessions, extensions, installer/uninstaller logic, or triaging regressions in this repo.
---

<!-- pi_agent_rust installer managed skill -->

# Pi Agent Rust

## Use This Skill When

- You are working inside `pi_agent_rust` and need the fastest path to safe, verified edits.
- You are touching provider/tool/session/extension behavior and need targeted triage.
- You are changing installer/uninstaller/skill install behavior and need deterministic safety checks.
- You need symptom-first debugging playbooks instead of ad-hoc command hunting.

## 60-Second Bootstrap

```bash
export CARGO_TARGET_DIR="/data/tmp/pi_agent_rust/${USER:-agent}"
export TMPDIR="/data/tmp/pi_agent_rust/${USER:-agent}/tmp"
mkdir -p "$TMPDIR"

rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings
cargo fmt --check
bash tests/installer_regression.sh
```

## Symptom Router

| Symptom | First 3 Commands |
|---|---|
| Provider stream/tool-call regression | `cargo test provider_streaming -- --nocapture` ; `rg -n "stream|tool|delta|event|SSE" src/providers src/sse.rs` ; `cargo test conformance` |
| Session replay/index drift | `cargo test session -- --nocapture` ; `rg -n "Session|save|open|index|jsonl|sqlite" src/session.rs src/session_index.rs` ; `cargo test conformance` |
| Extension policy/runtime failure | `cargo test extension -- --nocapture` ; `rg -n "policy|hostcall|capability|quickjs|deny|allow" src/extensions.rs src/extensions_js.rs` ; `cargo test conformance` |
| Installer/uninstaller/skill issue | `bash tests/installer_regression.sh` ; `rg -n "AGENT_SKILL_STATUS|CHECKSUM_STATUS|SIGSTORE_STATUS|COMPLETIONS_STATUS" install.sh` ; `rg -n "managed skill|expected skill directory|PIAR_AGENT_SKILL" uninstall.sh` |
| Interactive vs RPC divergence | `cargo test e2e_rpc -- --nocapture` ; `rg -n "interactive|rpc|stdin|event|session" src/main.rs src/interactive.rs src/rpc.rs` ; `cargo test conformance` |

For deeper diagnosis, use `references/DEBUGGING-PLAYBOOKS.md`.

## Non-Negotiables

- Read `AGENTS.md` first, then follow it exactly.
- Do not delete files or run destructive git/filesystem commands.
- Keep edits in-place; avoid creating variant files for the same purpose.
- Use `main` semantics in docs/scripts; do not introduce `master`.
- Prefer `rg` for fast text recon and `ast-grep` for structural matching/refactors.
- Prefer `rch exec -- <cargo ...>` for heavy compile/test workloads.
- After substantive edits, run compile/lint/format gates and the smallest relevant regression slice.

## Core Workflow

- [ ] Recon: identify exact change surface and invariants.
- [ ] Implement: minimal, behavior-focused patch with explicit failure semantics.
- [ ] Validate: targeted tests first, broaden only as needed.
- [ ] Verify UX: error/status output is explicit, stable, and non-ambiguous.
- [ ] Sync docs: update `README.md` when flags/behavior/user guidance changed.

## Changed Files -> Required Tests

| Changed Files (examples) | Minimum Required Tests |
|---|---|
| `install.sh`, `uninstall.sh`, `.claude/skills/pi-agent-rust/**` | `bash -n install.sh uninstall.sh tests/installer_regression.sh` ; `shellcheck -x install.sh uninstall.sh tests/installer_regression.sh` ; `bash tests/installer_regression.sh` ; `bash scripts/skill-smoke.sh` |
| `src/providers/**`, `src/provider.rs`, `src/sse.rs` | `cargo test provider_streaming` ; `cargo test conformance` |
| `src/session.rs`, `src/session_index.rs`, `src/session_test.rs` | `cargo test session` ; `cargo test conformance` |
| `src/extensions.rs`, `src/extensions_js.rs` | `cargo test extension` ; `cargo test conformance` |
| `src/tools.rs` | `cargo test tools` ; `cargo test conformance` |
| `src/interactive.rs`, `src/rpc.rs`, `src/main.rs` | `cargo test e2e_rpc` ; `cargo test conformance` |

## Do Not Run Yet

Run these only after targeted repro + focused slice indicates need:

- Broad `cargo test` across entire workspace when a narrower slice already reproduces.
- Heavy multi-surface runs before confirming changed-file impact.
- Repeated full conformance loops while the core failing slice is still unstable.

## High-Value Commands

```bash
# Fast recon
git status --short
rg -n "install|uninstall|skill|checksum|sigstore|completion|provider|session|extension" \
  install.sh uninstall.sh README.md tests/installer_regression.sh src/

# Installer + skill safety gates
bash -n install.sh uninstall.sh tests/installer_regression.sh
shellcheck -x install.sh uninstall.sh tests/installer_regression.sh
bash tests/installer_regression.sh
bash scripts/skill-smoke.sh

# Rust gates
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

For an expanded command cookbook, see `references/COMMANDS.md`.
For deep incident triage, see `references/DEBUGGING-PLAYBOOKS.md`.

## Critical Files

- `src/main.rs`: CLI entry and mode dispatch.
- `src/agent.rs`: agent loop and tool iteration behavior.
- `src/provider.rs`: provider trait contract.
- `src/providers/`: provider implementations and factory wiring.
- `src/tools.rs`: built-in tools (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`).
- `src/session.rs`: JSONL session persistence.
- `src/session_index.rs`: session index and metadata cache.
- `src/extensions.rs` + `src/extensions_js.rs`: extension policy and QuickJS bridge.
- `src/interactive.rs` + `src/rpc.rs`: TUI and RPC/stdin surfaces.
- `install.sh` + `uninstall.sh`: install lifecycle, migration, and skill management.
- `tests/installer_regression.sh`: installer regression harness.
- `scripts/skill-smoke.sh`: skill integrity + inline-sync validation.

## Known Footguns

- Custom artifact install paths without compatible release context can fall back incorrectly if not explicitly guarded.
- Skill status can become misleading on mixed outcomes unless partial/failure branches are explicit.
- Uninstall logic must enforce both marker checks and expected destination path shape.
- Installer progress/status text should stay on stderr when stdout is used for data plumbing.
- Bundled skill and inline fallback can silently drift unless explicitly checked.

## Patch Patterns

### Pattern 1: Mixed Outcome Status Clarity

```bash
# BEFORE: everything collapsed into "skipped custom"
if [ "$skipped_custom" -ge 1 ]; then
  AGENT_SKILL_STATUS="skipped (existing custom skill)"
fi

# AFTER: distinguish custom-skip from write failure
if [ "$skipped_custom" -ge 1 ] && [ "$failed_writes" -ge 1 ]; then
  AGENT_SKILL_STATUS="partial (custom skill kept; other install failed)"
elif [ "$skipped_custom" -ge 1 ]; then
  AGENT_SKILL_STATUS="skipped (existing custom skill)"
fi
```

### Pattern 2: Safe Skill Replacement

```bash
# BEFORE: remove destination before validating copy result
rm -rf "$destination"
cp "$source" "$destination/SKILL.md"

# AFTER: stage then atomically move into place
staged="$(mktemp -d ...)"
cp "$source" "$staged/SKILL.md"
mv "$staged" "$destination"
```

## Failure Triage

- Installer summary/status mismatch:
  trace `AGENT_SKILL_STATUS`, `CHECKSUM_STATUS`, and `COMPLETIONS_STATUS` in `install.sh`.
- Install/uninstall safety concern:
  verify marker checks and expected destination guards in both scripts.
- Provider/session/extension regressions:
  use symptom router, then follow `references/DEBUGGING-PLAYBOOKS.md`.
- Docs drift:
  ensure `README.md` flags/examples match current installer behavior.

## Done Criteria

- Changed-file matrix minimum tests passed.
- Compile/lint/format checks passed for touched surfaces.
- Installer/skill changes pass `tests/installer_regression.sh` and `scripts/skill-smoke.sh`.
- Behavior is explicit on failure paths; no silent fallback surprises.
- Skill docs and inline fallback remain aligned and current.
SKILL
}

pi_agent_skill_commands_reference_content() {
  cat <<'COMMANDS'
# Command Recipes

## 1) Session Bootstrap

```bash
export CARGO_TARGET_DIR="/data/tmp/pi_agent_rust/${USER:-agent}"
export TMPDIR="/data/tmp/pi_agent_rust/${USER:-agent}/tmp"
mkdir -p "$TMPDIR"
```

## 2) Fast Recon

```bash
git status --short
rg -n "TODO|FIXME|install|uninstall|skill|checksum|sigstore|completion|provider|session|extension" \
  install.sh uninstall.sh README.md tests/installer_regression.sh src/
```

## 3) Skill Integrity and Sync Guard

```bash
bash scripts/skill-smoke.sh
```

## 4) Installer and Shell Safety Gates

```bash
bash -n install.sh uninstall.sh tests/installer_regression.sh
shellcheck -x install.sh uninstall.sh tests/installer_regression.sh
bash tests/installer_regression.sh
```

## 5) Rust Quality Gates

```bash
# Preferred in multi-agent environments
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings

# Formatting can run locally
cargo fmt --check
```

## 6) Targeted Test Slices

```bash
# Tool behavior
cargo test tools

# Provider streaming/protocol
cargo test provider_streaming

# Session persistence/index
cargo test session

# Extension runtime/policy
cargo test extension

# RPC surface
cargo test e2e_rpc

# Broader safety net after targeted slices
cargo test conformance
```

## 7) Focused Installer Branch Debugging

```bash
# Help/flag visibility
bash install.sh --help

# Explicit custom artifact path (replace values for real run)
bash install.sh --yes --offline --version v0.0.0 \
  --artifact-url "file:///tmp/pi-artifact" \
  --checksum "<sha256>" \
  --no-completions --no-agent-skills

# Uninstall smoke
bash uninstall.sh --yes --no-gum
```

## 8) Status and Safety Tracing

```bash
# Installer status flow
rg -n "AGENT_SKILL_STATUS|CHECKSUM_STATUS|SIGSTORE_STATUS|COMPLETIONS_STATUS" install.sh

# Skill install safety + replacement
rg -n "install_skill_to_destination|is_installer_managed_skill_file|is_expected_skill_destination" install.sh

# Uninstall safety guards
rg -n "remove_installed_skills|is_managed_skill_file|is_expected_skill_directory|PIAR_AGENT_SKILL" uninstall.sh
```

## 9) Docs Drift Checks

```bash
rg -n "no-agent-skills|completions|checksum|sigstore|artifact-url|skill" README.md install.sh uninstall.sh
```

For symptom-driven step-by-step root-cause flows, use `DEBUGGING-PLAYBOOKS.md`.
COMMANDS
}

pi_agent_skill_debugging_playbooks_content() {
  cat <<'DEBUGGING'
# Debugging Playbooks

Use this file when the symptom is known but the root cause is unclear.
Each playbook is symptom-first and ends with a concrete fix verification checklist.

## Error Signature Map

| Error Signature (example) | Likely Root Area | First Command |
|---|---|---|
| `Release checksum verification failed; aborting install` | Checksum source/manifest parsing in installer | `rg -n "checksum|SHA256SUMS|CHECKSUM_STATUS" install.sh tests/installer_regression.sh` |
| `Release signature verification failed; aborting install` | Sigstore/cosign path or bundle handling | `rg -n "sigstore|cosign|SIGSTORE_STATUS" install.sh tests/installer_regression.sh` |
| `Custom artifact download failed; cannot fall back to source` | Synthetic custom artifact flow | `rg -n "custom-artifact|artifact-url|fall back to source" install.sh tests/installer_regression.sh` |
| `Skills:    partial (...)` | Mixed skill-install outcome logic | `rg -n "install_agent_skills|AGENT_SKILL_STATUS|failed_writes|skipped_custom" install.sh` |
| `Skipping unexpected skill directory path:` | Uninstall path guard triggered | `rg -n "is_expected_skill_directory|remove_installed_skills" uninstall.sh` |
| Streaming/tool-call mismatch in provider tests | Provider streaming/event normalization | `cargo test provider_streaming -- --nocapture` |
| Session replay/index drift | Session persistence/index metadata logic | `cargo test session -- --nocapture` |
| Extension hostcall/capability denial mismatch | Extension policy + QuickJS bridge | `cargo test extension -- --nocapture` |

## Playbook 1: Provider Streaming / Tool-Call Regressions

### Symptoms
- Streaming stalls, truncates, or emits malformed deltas.
- Tool-call events differ between provider backends.
- Provider streaming tests fail after provider or parser edits.

### First 3 Commands

```bash
cargo test provider_streaming -- --nocapture
rg -n "stream|tool|delta|event|SSE|responses|completions" src/providers src/provider.rs src/sse.rs
cargo test conformance
```

### Minimal Repro Template

```bash
# Replace with the narrowest failing test name from provider_streaming output.
cargo test provider_streaming::<failing_case> -- --nocapture
```

### Narrow the Change Surface

```bash
rg -n "impl Provider|stream|tool" src/providers/*.rs src/providers/mod.rs src/provider.rs
rg -n "parse|event|data:" src/sse.rs
```

### Fix Verification Checklist

- [ ] Failing provider streaming case reproduces before fix.
- [ ] Targeted provider test passes after fix.
- [ ] No regression in `cargo test conformance`.
- [ ] Error/status messaging remains explicit and unchanged unless intentional.

## Playbook 2: Session Persistence / Index Drift

### Symptoms
- Session replay/history differs unexpectedly between runs.
- Session index metadata mismatches stored entries.
- Save/open path behavior regresses after session changes.

### First 3 Commands

```bash
cargo test session -- --nocapture
rg -n "Session|save|open|index|jsonl|sqlite" src/session.rs src/session_index.rs src/session_test.rs
cargo test conformance
```

### Minimal Repro Template

```bash
# Replace with specific failing session test from output.
cargo test session::<failing_case> -- --nocapture
```

### Narrow the Change Surface

```bash
rg -n "append|save|open|diagnostic|branch|metadata" src/session.rs src/session_index.rs
```

### Fix Verification Checklist

- [ ] One deterministic session test reproduces before fix.
- [ ] Session test slice passes after fix.
- [ ] Conformance remains green for touched behavior.
- [ ] No undocumented format/semantic drift introduced.

## Playbook 3: Extension Runtime / Policy Regressions

### Symptoms
- Hostcalls denied/allowed unexpectedly.
- Capability policies behave inconsistently by profile.
- QuickJS runtime behavior diverges from expected policy enforcement.

### First 3 Commands

```bash
cargo test extension -- --nocapture
rg -n "extension|policy|hostcall|capability|quickjs|security|deny|allow" src/extensions.rs src/extensions_js.rs tests/
cargo test conformance
```

### Minimal Repro Template

```bash
# Replace with specific failing extension test from output.
cargo test extension::<failing_case> -- --nocapture
```

### Narrow the Change Surface

```bash
rg -n "allow|deny|policy|capability|hostcall" src/extensions.rs src/extensions_js.rs
```

### Fix Verification Checklist

- [ ] Failing extension test reproduces before fix.
- [ ] Targeted extension slice passes after fix.
- [ ] Policy semantics are still least-privilege and explicit.
- [ ] Broader conformance remains stable.

## Playbook 4: Installer / Uninstaller / Skill Installation Failures

### Symptoms
- Installer summary status is wrong, vague, or contradictory.
- Existing custom skill directories are modified unexpectedly.
- Uninstall removes unintended paths.
- Checksum/signature/completion branches regress.

### First 3 Commands

```bash
bash tests/installer_regression.sh
rg -n "AGENT_SKILL_STATUS|CHECKSUM_STATUS|SIGSTORE_STATUS|COMPLETIONS_STATUS|install_skill_to_destination" install.sh
rg -n "remove_installed_skills|is_expected_skill_directory|is_managed_skill_file|PIAR_AGENT_SKILL" uninstall.sh
```

### Minimal Repro Template

```bash
# Run a single installer regression case by editing tests/installer_regression.sh
# to isolate the failing test, then:
bash tests/installer_regression.sh

# Skill integrity + inline sync guard:
bash scripts/skill-smoke.sh
```

### Narrow the Change Surface

```bash
rg -n "install_skill_to_destination|install_agent_skills|write_state|print_summary" install.sh
rg -n "remove_installed_skills|is_expected_skill_directory|is_managed_skill_file" uninstall.sh
```

### Fix Verification Checklist

- [ ] Failing installer regression case reproduces before fix.
- [ ] `bash tests/installer_regression.sh` passes after fix.
- [ ] `bash scripts/skill-smoke.sh` passes after fix.
- [ ] Custom-skill preservation and managed-only deletion behavior remains intact.

## Playbook 5: CLI/TUI vs RPC Divergence

### Symptoms
- Interactive mode works while RPC/stdin mode fails (or inverse).
- Event ordering/shape differs by surface.

### First 3 Commands

```bash
cargo test e2e_rpc -- --nocapture
rg -n "interactive|rpc|stdin|event|session" src/main.rs src/interactive.rs src/rpc.rs
cargo test conformance
```

### Minimal Repro Template

```bash
# Replace with specific failing RPC test from output.
cargo test e2e_rpc::<failing_case> -- --nocapture
```

### Fix Verification Checklist

- [ ] Failing RPC case reproduces before fix.
- [ ] Targeted RPC test passes after fix.
- [ ] Event behavior remains consistent across surfaces.
- [ ] Broader conformance still passes.

## Standard Escalation Path

Use this only after targeted repro + narrow slice:

```bash
# 1) targeted failing slice first
cargo test <targeted-slice> -- --nocapture

# 2) local invariants for changed code
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets -- -D warnings
cargo fmt --check

# 3) broader behavior signal
cargo test conformance
```

## Root-Cause Confirmation Checklist

- [ ] Reproduced the failure with a deterministic command.
- [ ] Identified one minimal change that explains the symptom.
- [ ] Added or updated regression coverage for the fixed path.
- [ ] Verified failure before fix and pass after fix on targeted slice.
- [ ] Ran broader safety gates for touched surface.
- [ ] Updated docs/skill guidance if user-visible behavior changed.
DEBUGGING
}

pi_agent_skill_relative_paths() {
  cat <<'EOF'
SKILL.md
references/COMMANDS.md
references/DEBUGGING-PLAYBOOKS.md
EOF
}

pi_agent_skill_emit_file() {
  local relative_path="$1"
  case "$relative_path" in
    SKILL.md)
      pi_agent_skill_inline_content
      ;;
    references/COMMANDS.md)
      pi_agent_skill_commands_reference_content
      ;;
    references/DEBUGGING-PLAYBOOKS.md)
      pi_agent_skill_debugging_playbooks_content
      ;;
    *)
      return 1
      ;;
  esac
}

pi_agent_skill_stage_inline_tree() {
  local destination="$1"
  local relative_path=""
  local target_path=""

  if ! mkdir -p "$destination" 2>/dev/null; then
    return 1
  fi

  while IFS= read -r relative_path; do
    [ -n "$relative_path" ] || continue
    target_path="$destination/$relative_path"
    if ! mkdir -p "$(dirname "$target_path")" 2>/dev/null; then
      return 1
    fi
    if ! pi_agent_skill_emit_file "$relative_path" > "$target_path"; then
      return 1
    fi
  done < <(pi_agent_skill_relative_paths)

  [ -f "$destination/SKILL.md" ]
}

pi_agent_skill_stage_github_tree() {
  local ref="$1"
  local destination="$2"
  local relative_path=""
  local target_path=""
  local skill_url=""

  if ! mkdir -p "$destination" 2>/dev/null; then
    return 1
  fi

  while IFS= read -r relative_path; do
    [ -n "$relative_path" ] || continue
    target_path="$destination/$relative_path"
    if ! mkdir -p "$(dirname "$target_path")" 2>/dev/null; then
      return 1
    fi
    skill_url="https://raw.githubusercontent.com/${OWNER}/${REPO}/${ref}/.claude/skills/${AGENT_SKILL_NAME}/${relative_path}"
    if ! fetch_url_to_file "$skill_url" "$target_path" "agent skill" >/dev/null 2>&1; then
      return 1
    fi
  done < <(pi_agent_skill_relative_paths)

  [ -f "$destination/SKILL.md" ]
}

install_skill_to_destination() {
  local destination="$1"
  local source_kind="$2"
  local source_path="$3"

  if ! is_expected_skill_destination "$destination"; then
    warn "Skipping unexpected skill destination path: $destination"
    return 1
  fi

  case "$source_kind" in
    dir)
      if [ ! -d "$source_path" ] || [ ! -f "$source_path/SKILL.md" ]; then
        warn "Invalid bundled skill source directory: $source_path"
        return 1
      fi
      ;;
    file)
      if [ ! -f "$source_path" ]; then
        warn "Missing skill source file: $source_path"
        return 1
      fi
      ;;
    *)
      warn "Unknown skill source kind: $source_kind"
      return 1
      ;;
  esac

  local existing_skill="$destination/SKILL.md"
  if [ -e "$destination" ] && [ ! -f "$existing_skill" ]; then
    warn "Skipping existing skill directory without SKILL.md at $destination"
    return 2
  fi
  if [ -f "$existing_skill" ] && ! is_installer_managed_skill_file "$existing_skill"; then
    warn "Skipping existing non-installer-managed skill at $destination"
    return 2
  fi

  local destination_parent
  destination_parent="$(dirname "$destination")"
  if ! mkdir -p "$destination_parent" 2>/dev/null; then
    warn "Failed to create skill parent directory: $destination_parent"
    return 1
  fi

  local staged_destination=""
  staged_destination="$(mktemp -d "${destination_parent}/.${AGENT_SKILL_NAME}.tmp.XXXXXX" 2>/dev/null || true)"
  if [ -z "$staged_destination" ] || [ ! -d "$staged_destination" ]; then
    warn "Failed to create staging directory for skill install: $destination"
    return 1
  fi

  case "$source_kind" in
    dir)
      if ! cp -R "$source_path/." "$staged_destination/" 2>/dev/null; then
        warn "Failed to install bundled skill into $destination"
        remove_path_recursively "$staged_destination" 2>/dev/null || true
        return 1
      fi
      ;;
    file)
      if ! cp "$source_path" "$staged_destination/SKILL.md" 2>/dev/null; then
        warn "Failed to install skill file into $destination"
        remove_path_recursively "$staged_destination" 2>/dev/null || true
        return 1
      fi
      ;;
  esac

  if [ ! -f "$staged_destination/SKILL.md" ]; then
    warn "Skill install failed: missing SKILL.md at $destination"
    remove_path_recursively "$staged_destination" 2>/dev/null || true
    return 1
  fi

  if ! is_installer_managed_skill_file "$staged_destination/SKILL.md"; then
    if ! printf '\n<!-- %s -->\n' "$AGENT_SKILL_MARKER" >> "$staged_destination/SKILL.md"; then
      warn "Failed to mark skill as installer-managed at $destination"
      remove_path_recursively "$staged_destination" 2>/dev/null || true
      return 1
    fi
  fi

  if ! is_installer_managed_skill_file "$staged_destination/SKILL.md"; then
    warn "Skill install failed: managed marker missing at $destination"
    remove_path_recursively "$staged_destination" 2>/dev/null || true
    return 1
  fi

  if [ -e "$destination" ] || [ -L "$destination" ]; then
    if ! remove_path_recursively "$destination" 2>/dev/null; then
      warn "Failed to replace existing skill directory: $destination"
      remove_path_recursively "$staged_destination" 2>/dev/null || true
      return 1
    fi
  fi
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    warn "Failed to clear existing skill directory: $destination"
    remove_path_recursively "$staged_destination" 2>/dev/null || true
    return 1
  fi

  if ! mv "$staged_destination" "$destination" 2>/dev/null; then
    warn "Failed to move staged skill into place: $destination"
    remove_path_recursively "$staged_destination" 2>/dev/null || true
    return 1
  fi

  return 0
}

install_agent_skills() {
  if [ "$AGENT_SKILLS_ENABLED" -eq 0 ]; then
    AGENT_SKILL_STATUS="skipped (--no-agent-skills)"
    return 0
  fi

  local codex_home="${CODEX_HOME:-$HOME/.codex}"
  AGENT_SKILL_CLAUDE_PATH="$HOME/.claude/skills/${AGENT_SKILL_NAME}"
  AGENT_SKILL_CODEX_PATH="${codex_home}/skills/${AGENT_SKILL_NAME}"

  local source_kind="dir"
  local source_path=""
  local source_desc="inline"
  local bundled_dir=""

  local script_dir=""
  local script_dir_candidate=""
  if script_dir_candidate="$(cd "$(dirname "${BASH_SOURCE[0]:-}")" 2>/dev/null && pwd -P)"; then
    script_dir="$script_dir_candidate"
  fi
  local bundled_candidate=""
  bundled_candidate="$script_dir/.claude/skills/${AGENT_SKILL_NAME}"
  if [ -f "$bundled_candidate/SKILL.md" ]; then
    bundled_dir="$bundled_candidate"
  fi

  local temp_skill_dir=""
  if [ -n "$bundled_dir" ]; then
    source_kind="dir"
    source_path="$bundled_dir"
    source_desc="bundled"
  else
    if command -v curl >/dev/null 2>&1; then
      local refs=()
      if [ -n "$VERSION" ] && [ "$VERSION" != "custom-artifact" ]; then
        refs+=("$VERSION")
      fi
      refs+=("main")
      local prev_ref=""
      local ref=""
      for ref in "${refs[@]}"; do
        if [ -n "$prev_ref" ] && [ "$ref" = "$prev_ref" ]; then
          continue
        fi
        prev_ref="$ref"
        local downloaded_dir=""
        downloaded_dir="$(mktemp -d 2>/dev/null || true)"
        if [ -z "$downloaded_dir" ] || [ ! -d "$downloaded_dir" ]; then
          break
        fi
        if pi_agent_skill_stage_github_tree "$ref" "$downloaded_dir"; then
          temp_skill_dir="$downloaded_dir"
          source_path="$downloaded_dir"
          source_desc="github:${ref}"
          break
        fi
        remove_path_recursively "$downloaded_dir" 2>/dev/null || true
      done
    fi

    if [ -z "$source_path" ]; then
      local inline_skill_dir=""
      inline_skill_dir="$(mktemp -d 2>/dev/null || true)"
      if [ -z "$inline_skill_dir" ] || [ ! -d "$inline_skill_dir" ]; then
        AGENT_SKILL_STATUS="failed (temp dir error)"
        warn "Failed to prepare inline agent skill directory"
        return 0
      fi
      if ! pi_agent_skill_stage_inline_tree "$inline_skill_dir"; then
        AGENT_SKILL_STATUS="failed (inline skill staging error)"
        warn "Failed to stage inline agent skill directory"
        remove_path_recursively "$inline_skill_dir" 2>/dev/null || true
        return 0
      fi
      temp_skill_dir="$inline_skill_dir"
      source_path="$inline_skill_dir"
      source_desc="inline"
    fi
  fi

  local installed_claude=0
  local installed_codex=0
  local skipped_custom=0
  local failed_writes=0
  local install_rc=0

  if install_skill_to_destination "$AGENT_SKILL_CLAUDE_PATH" "$source_kind" "$source_path"; then
    installed_claude=1
  else
    install_rc=$?
    if [ "$install_rc" -eq 2 ]; then
      skipped_custom=$((skipped_custom + 1))
    else
      failed_writes=$((failed_writes + 1))
    fi
  fi

  if install_skill_to_destination "$AGENT_SKILL_CODEX_PATH" "$source_kind" "$source_path"; then
    installed_codex=1
  else
    install_rc=$?
    if [ "$install_rc" -eq 2 ]; then
      skipped_custom=$((skipped_custom + 1))
    else
      failed_writes=$((failed_writes + 1))
    fi
  fi

  if [ "$installed_claude" -eq 1 ] && [ "$installed_codex" -eq 1 ]; then
    AGENT_SKILL_STATUS="installed (claude,codex)"
    ok "Installed ${AGENT_SKILL_NAME} skill for Claude and Codex (${source_desc})"
    return 0
  fi
  if [ "$installed_claude" -eq 1 ] && [ "$installed_codex" -eq 0 ]; then
    if [ "$failed_writes" -ge 1 ]; then
      AGENT_SKILL_STATUS="partial (claude installed; codex failed)"
      warn "Installed ${AGENT_SKILL_NAME} skill for Claude, but Codex install failed (${source_desc})"
    elif [ "$skipped_custom" -ge 1 ]; then
      AGENT_SKILL_STATUS="installed (claude only; codex custom kept)"
      warn "Installed ${AGENT_SKILL_NAME} skill for Claude; kept existing custom Codex skill (${source_desc})"
    else
      AGENT_SKILL_STATUS="installed (claude only)"
      warn "Installed ${AGENT_SKILL_NAME} skill for Claude only (${source_desc})"
    fi
    return 0
  fi
  if [ "$installed_claude" -eq 0 ] && [ "$installed_codex" -eq 1 ]; then
    if [ "$failed_writes" -ge 1 ]; then
      AGENT_SKILL_STATUS="partial (codex installed; claude failed)"
      warn "Installed ${AGENT_SKILL_NAME} skill for Codex, but Claude install failed (${source_desc})"
    elif [ "$skipped_custom" -ge 1 ]; then
      AGENT_SKILL_STATUS="installed (codex only; claude custom kept)"
      warn "Installed ${AGENT_SKILL_NAME} skill for Codex; kept existing custom Claude skill (${source_desc})"
    else
      AGENT_SKILL_STATUS="installed (codex only)"
      warn "Installed ${AGENT_SKILL_NAME} skill for Codex only (${source_desc})"
    fi
    return 0
  fi

  if [ "$skipped_custom" -ge 1 ] && [ "$failed_writes" -ge 1 ]; then
    AGENT_SKILL_STATUS="partial (custom skill kept; other install failed)"
    warn "Kept existing custom skill at one destination; install failed at another (${source_desc})"
  elif [ "$skipped_custom" -ge 1 ]; then
    AGENT_SKILL_STATUS="skipped (existing custom skill)"
  else
    AGENT_SKILL_STATUS="failed (unable to write skill files)"
  fi

  if [ -n "$temp_skill_dir" ] && [ -d "$temp_skill_dir" ]; then
    remove_path_recursively "$temp_skill_dir" 2>/dev/null || true
  fi
}

load_existing_state() {
  if [ -f "$STATE_FILE" ]; then
    # shellcheck disable=SC1090
    source "$STATE_FILE"
  fi
}

write_state() {
  mkdir -p "$STATE_DIR"
  {
    printf '# pi_agent_rust installer state\n'
    printf 'PIAR_STATE_VERSION=%q\n' "$STATE_VERSION"
    printf 'PIAR_INSTALL_VERSION=%q\n' "$VERSION"
    printf 'PIAR_INSTALL_SOURCE=%q\n' "$INSTALL_SOURCE"
    printf 'PIAR_INSTALL_DEST=%q\n' "$DEST"
    printf 'PIAR_INSTALL_BIN=%q\n' "$INSTALL_BIN_PATH"
    printf 'PIAR_INSTALL_BIN_NAME=%q\n' "$FINAL_BIN_NAME"
    printf 'PIAR_CHECKSUM_STATUS=%q\n' "$CHECKSUM_STATUS"
    printf 'PIAR_SIGSTORE_STATUS=%q\n' "$SIGSTORE_STATUS"
    printf 'PIAR_COMPLETIONS_STATUS=%q\n' "$COMPLETIONS_STATUS"
    printf 'PIAR_AGENT_SKILL_STATUS=%q\n' "$AGENT_SKILL_STATUS"
    printf 'PIAR_AGENT_SKILL_CLAUDE_PATH=%q\n' "$AGENT_SKILL_CLAUDE_PATH"
    printf 'PIAR_AGENT_SKILL_CODEX_PATH=%q\n' "$AGENT_SKILL_CODEX_PATH"
    printf 'PIAR_ADOPTED_TYPESCRIPT=%q\n' "$ADOPT_TS"
    printf 'PIAR_LEGACY_ALIAS_PATH=%q\n' "$LEGACY_ALIAS_PATH"
    printf 'PIAR_LEGACY_TARGET_PATH=%q\n' "$LEGACY_TARGET_PATH"
    printf 'PIAR_LEGACY_MOVED_FROM=%q\n' "$LEGACY_MOVED_FROM"
    printf 'PIAR_LEGACY_MOVED_TO=%q\n' "$LEGACY_MOVED_TO"
    printf 'PIAR_COMPAT_ALIAS_PATH=%q\n' "$COMPAT_ALIAS_PATH"
    printf 'PIAR_COMPAT_ALIAS_STATUS=%q\n' "$COMPAT_ALIAS_STATUS"
    printf 'PIAR_PATH_MARKER=%q\n' "$PATH_MARKER"
    printf 'PIAR_PATH_UPDATED_FILES=%q\n' "$PATH_UPDATED_FILES"
    printf 'PIAR_INSTALLED_AT_UTC=%q\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } > "$STATE_FILE"
}

should_skip_reinstall() {
  if [ "$FORCE_INSTALL" -eq 1 ]; then
    return 1
  fi

  if [ ! -x "$INSTALL_BIN_PATH" ]; then
    return 1
  fi

  local out
  out="$(capture_version_line "$INSTALL_BIN_PATH")"
  if ! is_rust_pi_output "$out"; then
    return 1
  fi

  if [ -n "${PIAR_INSTALL_VERSION:-}" ] && [ "$PIAR_INSTALL_VERSION" = "$VERSION" ]; then
    return 0
  fi

  return 1
}

print_summary() {
  [ "$QUIET" -eq 1 ] && return 0

  local lines=()
  lines+=("Installed: $INSTALL_BIN_PATH")
  lines+=("Version:   $VERSION")
  lines+=("Source:    $INSTALL_SOURCE")
  lines+=("Checksum:  $CHECKSUM_STATUS")
  lines+=("Signature: $SIGSTORE_STATUS")
  lines+=("Shell:     $COMPLETIONS_STATUS")
  if [ -n "$PROXY_SOURCE" ]; then
    lines+=("Proxy:     $(redact_proxy_value "$PROXY_SOURCE")")
  fi
  if [ "$WSL_DETECTED" -eq 1 ]; then
    lines+=("Platform:  WSL detected")
  fi
  if [ "$COMPAT_ALIAS_STATUS" != "pending" ]; then
    lines+=("Alias:     $COMPAT_ALIAS_STATUS")
  fi
  lines+=("Skills:    $AGENT_SKILL_STATUS")
  if [ -n "$AGENT_SKILL_CLAUDE_PATH" ] && [ -f "$AGENT_SKILL_CLAUDE_PATH/SKILL.md" ]; then
    lines+=("Claude:    $AGENT_SKILL_CLAUDE_PATH")
  fi
  if [ -n "$AGENT_SKILL_CODEX_PATH" ] && [ -f "$AGENT_SKILL_CODEX_PATH/SKILL.md" ]; then
    lines+=("Codex:     $AGENT_SKILL_CODEX_PATH")
  fi

  if [ "$ADOPT_TS" -eq 1 ]; then
    if [ "$ADOPT_CANONICAL" -eq 1 ]; then
      lines+=("Mode:      Rust is canonical 'pi'")
    else
      lines+=("Mode:      Adoption requested; ensure '$DEST' precedes existing pi in PATH")
    fi
    if [ -n "$LEGACY_ALIAS_PATH" ]; then
      lines+=("Legacy:    $(basename "$LEGACY_ALIAS_PATH") -> $LEGACY_TARGET_PATH")
    fi
  elif [ "$FINAL_BIN_NAME" = "pi-rust" ]; then
    lines+=("Mode:      Existing pi kept; Rust installed as pi-rust")
  fi

  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    {
      gum style --foreground 42 --bold "pi installed successfully"
      echo ""
      for line in "${lines[@]}"; do
        gum style --foreground 245 "$line"
      done
      echo ""
      gum style --foreground 245 "Uninstall: curl -fsSL https://raw.githubusercontent.com/${OWNER}/${REPO}/main/uninstall.sh | bash"
    } | gum style --border normal --border-foreground 42 --padding "1 2"
  else
    echo -e "\033[0;36m+------------------------------------------------------------------+\033[0m"
    echo -e "\033[1;32m| Pi Rust installed successfully                                   |\033[0m"
    echo -e "\033[0;36m+------------------------------------------------------------------+\033[0m"
    for line in "${lines[@]}"; do
      echo -e "  \033[0;37m$line\033[0m"
    done
    echo ""
    echo -e "  \033[0;90mUninstall: curl -fsSL https://raw.githubusercontent.com/${OWNER}/${REPO}/main/uninstall.sh | bash\033[0m"
  fi
}

main() {
  validate_options
  load_existing_state
  setup_proxy
  resolve_version
  show_header
  if [ "$OFFLINE" -eq 1 ] && [ -n "$OFFLINE_TARBALL" ]; then
    info "Offline artifact mode enabled: $OFFLINE_TARBALL"
  fi
  detect_existing_pi
  choose_adoption_mode
  choose_dest_for_adoption

  detect_platform
  prepare_asset_urls
  ensure_dest_dir
  ensure_install_target
  check_dependencies
  preflight_checks

  if should_skip_reinstall; then
    INSTALL_SOURCE="existing (no reinstall)"
    CHECKSUM_STATUS="not run (already installed)"
    SIGSTORE_STATUS="not run (already installed)"
    ok "pi ${VERSION} already installed at $INSTALL_BIN_PATH"
    if [ "$ADOPT_TS" -eq 1 ]; then
      local refresh_legacy=0
      if [ -z "${PIAR_LEGACY_ALIAS_PATH:-}" ]; then
        refresh_legacy=1
      elif [ ! -f "${PIAR_LEGACY_ALIAS_PATH}" ]; then
        refresh_legacy=1
      elif ! grep -q "pi_agent_rust installer managed alias" "${PIAR_LEGACY_ALIAS_PATH}" 2>/dev/null; then
        refresh_legacy=1
      fi

      if [ "$refresh_legacy" -eq 1 ]; then
        prepare_typescript_migration
        write_state
      fi
    fi
    ensure_compat_alias
    maybe_add_path
    maybe_install_completions
    cleanup_legacy_agent_settings
    install_agent_skills
    write_state
    print_summary
    return 0
  fi

  acquire_lock
  TMP=$(mktemp -d)

  local source_bin=""
  if [ "$FROM_SOURCE" -eq 1 ]; then
    INSTALL_SOURCE="source"
    CHECKSUM_STATUS="not applicable (source build)"
    SIGSTORE_STATUS="not applicable (source build)"
    run_with_spinner "Building pi from source" build_from_source > "$TMP/source_bin_path"
    source_bin=$(cat "$TMP/source_bin_path")
  else
    INSTALL_SOURCE="release"
    local download_rc=0
    if run_with_spinner "Downloading release binary" download_release_binary > "$TMP/source_bin_path"; then
      source_bin=$(cat "$TMP/source_bin_path")
    else
      download_rc=$?
      if [ "$download_rc" -eq 2 ] || [ "$download_rc" -eq 3 ] || [ "$download_rc" -eq 4 ]; then
        err "Release checksum verification failed; aborting install"
        exit 1
      fi
      if [ "$download_rc" -eq 5 ]; then
        err "Release signature verification failed; aborting install"
        exit 1
      fi
      if [ -n "$ARTIFACT_URL" ] && [ "$VERSION" = "custom-artifact" ]; then
        err "Custom artifact download failed; cannot fall back to source without a release tag"
        err "Pass --version vX.Y.Z with --artifact-url, or use --from-source directly"
        exit 1
      fi
      if [ "$OFFLINE" -eq 1 ]; then
        err "Offline mode download failed; source fallback is disabled"
        exit 1
      fi
      warn "Release download failed; falling back to source build"
      FROM_SOURCE=1
      INSTALL_SOURCE="source (release fallback)"
      CHECKSUM_STATUS="not applicable (source fallback)"
      SIGSTORE_STATUS="not applicable (source fallback)"
      check_dependencies
      run_with_spinner "Building pi from source" build_from_source > "$TMP/source_bin_path"
      source_bin=$(cat "$TMP/source_bin_path")
    fi
  fi

  prepare_typescript_migration
  install_binary_file "$source_bin"
  ensure_compat_alias

  if [ "$VERIFY" -eq 1 ]; then
    "$INSTALL_BIN_PATH" --version >/dev/null
    ok "Verification passed ($FINAL_BIN_NAME --version)"
    if [ -n "$COMPAT_ALIAS_PATH" ]; then
      "$COMPAT_ALIAS_PATH" --version >/dev/null
      ok "Verification passed ($(basename "$COMPAT_ALIAS_PATH") --version)"
    fi
  fi

  maybe_add_path
  maybe_install_completions
  cleanup_legacy_agent_settings
  install_agent_skills
  write_state
  INSTALL_COMMITTED=1
  print_summary
}

if [[ "${BASH_SOURCE[0]:-}" == "${0:-}" ]] || [[ -z "${BASH_SOURCE[0]:-}" ]]; then
  main "$@"
fi
