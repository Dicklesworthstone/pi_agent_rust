#!/usr/bin/env bash
# scripts/e2e/run_ssh_workspace.sh — Gated live lane for ssh:// workspace
# tools (bd-cv653.6.5).
#
# Owns the fixture so the Rust target stays unsafe-free: generates dedicated
# host/user keys, boots a userspace /usr/sbin/sshd on 127.0.0.1, writes a
# scoped OpenSSH client config, then exports the router env and runs
# tests/e2e_ssh_workspace.rs against it.
#
# Scratch artifacts live under the OS temp dir (unique per run) and are left
# in place deliberately; the sshd process is always reaped via trap.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
ARTIFACT_DIR="${E2E_ARTIFACT_DIR:-$PROJECT_ROOT/tests/e2e_results/ssh/$STAMP}"
mkdir -p "$ARTIFACT_DIR"

CORRELATION_ID="${CI_CORRELATION_ID:-ssh-$STAMP}"
export CI_CORRELATION_ID="$CORRELATION_ID"

SSHD_BIN="/usr/sbin/sshd"
[[ -x "$SSHD_BIN" ]] || { echo "[ssh-e2e] no sshd binary; lane unavailable" >&2; exit 2; }

FIXTURE="$(mktemp -d "${TMPDIR:-/tmp}/pi-ssh-e2e-fixture.XXXXXX")"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/pi-ssh-e2e-work.XXXXXX")"

PORT="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"

cleanup() {
    if [[ -n "${SSHD_PID:-}" ]] && kill -0 "$SSHD_PID" 2>/dev/null; then
        kill "$SSHD_PID" 2>/dev/null || true
        wait "$SSHD_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "[ssh-e2e] generating fixture keys"
ssh-keygen -q -t ed25519 -N "" -f "$FIXTURE/hostkey"
ssh-keygen -q -t ed25519 -N "" -f "$FIXTURE/userkey"
cp "$FIXTURE/userkey.pub" "$FIXTURE/authorized_keys"

cat > "$FIXTURE/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $FIXTURE/hostkey
PidFile $FIXTURE/sshd.pid
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile $FIXTURE/authorized_keys
StrictModes no
LogLevel ERROR
EOF

USER_NAME="$(whoami)"
cat > "$FIXTURE/client_config" <<EOF
Host 127.0.0.1
    Port $PORT
    User $USER_NAME
    IdentityFile $FIXTURE/userkey
    IdentitiesOnly yes
    UserKnownHostsFile $FIXTURE/known_hosts
    GlobalKnownHostsFile /dev/null
    StrictHostKeyChecking accept-new
    BatchMode yes
EOF

echo "[ssh-e2e] booting userspace sshd on 127.0.0.1:$PORT"
"$SSHD_BIN" -D -e -f "$FIXTURE/sshd_config" &
SSHD_PID=$!

for _ in $(seq 1 60); do
    if python3 -c "import socket; socket.create_connection(('127.0.0.1', $PORT), timeout=0.25)" 2>/dev/null; then
        break
    fi
    sleep 0.05
done
if ! python3 -c "import socket; socket.create_connection(('127.0.0.1', $PORT), timeout=1)" 2>/dev/null; then
    echo "[ssh-e2e] fixture sshd never listened on port $PORT" >&2
    exit 3
fi

export PI_SSH_E2E=1
export PI_SSH_ALLOWED_HOSTS="127.0.0.1"
export PI_SSH_CLIENT_CONFIG_FILE="$FIXTURE/client_config"
export PI_SSH_E2E_WORK="$WORK"

echo "[ssh-e2e] running e2e_ssh_workspace (correlation: $CORRELATION_ID, work: $WORK)"
cargo test --test e2e_ssh_workspace -- --nocapture 2>&1 | tee "$ARTIFACT_DIR/run.log"

echo "[ssh-e2e] PASS (artifacts: $ARTIFACT_DIR; scratch retained at $WORK)"
