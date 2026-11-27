#!/usr/bin/env bash
# scripts/e2e/run_session_soak.sh — Explicit session-duration soak lane (GH #213, bd-brx7l).
#
# Runs outside the ordinary DSR quality lane so multi-turn duration soaks never
# slow or destabilize per-commit checks. Drives BOTH:
#   1. The real `pi` binary against a deterministic local loopback OpenAI-compatible
#      SSE stub server (zero network credentials required), recording every
#      warning/error emitted by the binary plus a sampled time series (RSS, open FDs,
#      child process count, session file bytes, per-turn latency).
#   2. Optionally (default: enabled unless --binary-only), the parameterized Rust
#      duration soak (`soak_session_duration_extended`) covering branching/linearity,
#      repeated compactions, persist/reload, and mutation-fault sensitivity.
#
# Usage:
#   scripts/e2e/run_session_soak.sh [options]
#
# Options:
#   --turns N             Number of turns for the soak run (default: 40; binary turns default: 12)
#   --binary-turns N      Number of turns driven through the real `pi` binary (default: 12)
#   --max-secs S          Optional wall-clock budget cap in seconds (default: unset)
#   --sample-interval K   Sample OS/session metrics every K turns (default: 4)
#   --mutation-fault M    Inject known leak fault: none | leak_fd | unbounded_vec (default: none)
#   --binary-only         Run only the real `pi` binary soak phase (skip cargo/rch Rust test)
#   --rust-only           Run only the parameterized Rust soak test (skip binary phase)
#   --pi-bin PATH         Explicit path to `pi` binary (default: auto-detect)
#   --artifact-dir DIR    Output directory (default: tests/e2e_results/session-soak/<run_id>)
#   --dry-run             Validate configuration & print execution plan without running
#   --help, -h            Show this help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

TURNS="${PI_SOAK_TURNS:-40}"
BINARY_TURNS="${PI_SOAK_BINARY_TURNS:-12}"
MAX_SECS="${PI_SOAK_MAX_SECS:-}"
SAMPLE_INTERVAL="${PI_SOAK_SAMPLE_INTERVAL:-4}"
MUTATION_FAULT="${PI_SOAK_MUTATION_FAULT:-none}"
RUN_BINARY=1
RUN_RUST=1
DRY_RUN=0
PI_BIN="${PI_BIN:-}"
ARTIFACT_DIR="${PI_SOAK_ARTIFACT_DIR:-}"
CARGO_RUNNER="${PI_SOAK_CARGO_RUNNER:-auto}"

require_arg() {
    if [[ "$1" -lt 2 ]]; then
        echo "Option $2 requires a value" >&2
        exit 2
    fi
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --turns)
            require_arg "$#" "$1"
            TURNS="$2"
            shift 2
            ;;
        --binary-turns)
            require_arg "$#" "$1"
            BINARY_TURNS="$2"
            shift 2
            ;;
        --max-secs)
            require_arg "$#" "$1"
            MAX_SECS="$2"
            shift 2
            ;;
        --sample-interval)
            require_arg "$#" "$1"
            SAMPLE_INTERVAL="$2"
            shift 2
            ;;
        --mutation-fault)
            require_arg "$#" "$1"
            MUTATION_FAULT="$2"
            shift 2
            ;;
        --binary-only)
            RUN_BINARY=1
            RUN_RUST=0
            shift
            ;;
        --rust-only)
            RUN_BINARY=0
            RUN_RUST=1
            shift
            ;;
        --pi-bin)
            require_arg "$#" "$1"
            PI_BIN="$2"
            shift 2
            ;;
        --artifact-dir)
            require_arg "$#" "$1"
            ARTIFACT_DIR="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --help|-h)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            exit 2
            ;;
    esac
done

case "$MUTATION_FAULT" in
    none|leak_fd|unbounded_vec) ;;
    *)
        echo "Invalid --mutation-fault: $MUTATION_FAULT (expected: none|leak_fd|unbounded_vec)" >&2
        exit 2
        ;;
esac

for num_var in TURNS BINARY_TURNS SAMPLE_INTERVAL; do
    val="${!num_var}"
    if [[ ! "$val" =~ ^[0-9]+$ ]] || [[ "$val" -eq 0 ]]; then
        echo "Invalid $num_var=$val (expected positive integer)" >&2
        exit 2
    fi
done
if [[ -n "$MAX_SECS" ]] && { [[ ! "$MAX_SECS" =~ ^[0-9]+$ ]] || [[ "$MAX_SECS" -eq 0 ]]; }; then
    echo "Invalid --max-secs=$MAX_SECS (expected positive integer)" >&2
    exit 2
fi

if [[ -z "$PI_BIN" ]]; then
    for candidate in \
        "$PROJECT_ROOT/target/dist/pi" \
        "$PROJECT_ROOT/target/release/pi" \
        "$PROJECT_ROOT/target/debug/pi" \
        "$(command -v pi 2>/dev/null || true)"; do
        if [[ -n "$candidate" && -x "$candidate" ]]; then
            PI_BIN="$candidate"
            break
        fi
    done
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
NONCE="$(python3 -c 'import secrets; print(secrets.token_hex(4))')"
RUN_ID="${STAMP}-${NONCE}"
if [[ -z "$ARTIFACT_DIR" ]]; then
    ARTIFACT_DIR="$PROJECT_ROOT/tests/e2e_results/session-soak/$RUN_ID"
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
    python3 - "$RUN_ID" "$ARTIFACT_DIR" "$PI_BIN" "$TURNS" "$BINARY_TURNS" "$MAX_SECS" "$SAMPLE_INTERVAL" "$MUTATION_FAULT" "$RUN_BINARY" "$RUN_RUST" <<'PY'
import json, sys
run_id, art_dir, pi_bin, turns, bin_turns, max_secs, interval, fault, run_bin, run_rust = sys.argv[1:]
print(json.dumps({
    "schema": "pi.soak.session_duration.plan.v1",
    "dry_run": True,
    "run_id": run_id,
    "artifact_dir": art_dir,
    "pi_bin": pi_bin or None,
    "turns": int(turns),
    "binary_turns": int(bin_turns),
    "max_secs": int(max_secs) if max_secs else None,
    "sample_interval": int(interval),
    "mutation_fault": fault,
    "run_binary_phase": run_bin == "1",
    "run_rust_phase": run_rust == "1",
}, indent=2))
PY
    exit 0
fi

mkdir -p "$ARTIFACT_DIR"
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd -P)"
GIT_SHA="$(git rev-parse HEAD 2>/dev/null || echo unknown)"

python3 - "$ARTIFACT_DIR/params.json" "$RUN_ID" "$GIT_SHA" "$PI_BIN" "$TURNS" "$BINARY_TURNS" "$MAX_SECS" "$SAMPLE_INTERVAL" "$MUTATION_FAULT" "$RUN_BINARY" "$RUN_RUST" <<'PY'
import json, sys
from datetime import datetime, timezone
out, run_id, sha, pi_bin, turns, bin_turns, max_secs, interval, fault, run_bin, run_rust = sys.argv[1:]
payload = {
    "schema": "pi.soak.session_duration.params.v1",
    "run_id": run_id,
    "started_at": datetime.now(timezone.utc).isoformat(),
    "git_sha": sha,
    "pi_bin": pi_bin or None,
    "turns": int(turns),
    "binary_turns": int(bin_turns),
    "max_secs": int(max_secs) if max_secs else None,
    "sample_interval": int(interval),
    "mutation_fault": fault,
    "run_binary_phase": run_bin == "1",
    "run_rust_phase": run_rust == "1",
}
with open(out, "w", encoding="utf-8") as fh:
    json.dump(payload, fh, indent=2)
    fh.write("\n")
PY

OVERALL_RC=0

if [[ "$RUN_BINARY" -eq 1 ]]; then
    if [[ -z "$PI_BIN" || ! -x "$PI_BIN" ]]; then
        echo "[session-soak] ERROR: no executable pi binary found (pass --pi-bin PATH)" >&2
        exit 2
    fi
    echo "[session-soak] Phase 1: Real pi binary duration soak ($BINARY_TURNS turns, pi=$PI_BIN)"
    if ! python3 - "$PI_BIN" "$ARTIFACT_DIR" "$BINARY_TURNS" "${MAX_SECS:-0}" "$SAMPLE_INTERVAL" "$MUTATION_FAULT" <<'PY'
import http.server
import json
import os
import queue
import socketserver
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

pi_bin = sys.argv[1]
artifact_dir = Path(sys.argv[2])
turns = int(sys.argv[3])
max_secs = int(sys.argv[4])
sample_interval = int(sys.argv[5])
mutation_fault = sys.argv[6]

request_counter = 0
counter_lock = threading.Lock()


class DeterministicOpenAIHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        return

    def do_POST(self):
        global request_counter
        length = int(self.headers.get("Content-Length", "0"))
        _ = self.rfile.read(length) if length > 0 else b""
        with counter_lock:
            request_counter += 1
            turn_id = request_counter

        reply_text = f"soak-binary-ack-turn-{turn_id}"
        chunk1 = {
            "id": f"chatcmpl-soak-{turn_id}",
            "object": "chat.completion.chunk",
            "created": int(time.time()),
            "model": "soak-stub-model",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": reply_text},
                "finish_reason": None,
            }],
        }
        chunk2 = {
            "id": f"chatcmpl-soak-{turn_id}",
            "object": "chat.completion.chunk",
            "created": int(time.time()),
            "model": "soak-stub-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 16, "completion_tokens": 8, "total_tokens": 24},
        }
        body = (
            f"data: {json.dumps(chunk1)}\n\n"
            f"data: {json.dumps(chunk2)}\n\n"
            "data: [DONE]\n\n"
        ).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)


def proc_metrics(pid: int) -> tuple[int, int, int]:
    rss_kb = 0
    open_fds = 0
    child_procs = 0
    try:
        for line in Path(f"/proc/{pid}/status").read_text(encoding="utf-8", errors="replace").splitlines():
            if line.startswith("VmRSS:"):
                parts = line.split()
                if len(parts) >= 2:
                    rss_kb = int(parts[1])
                break
    except OSError:
        pass
    try:
        open_fds = len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        pass
    try:
        task_dir = Path(f"/proc/{pid}/task")
        for tid in task_dir.iterdir():
            children = (tid / "children").read_text(encoding="utf-8", errors="replace").split()
            child_procs += len(children)
    except OSError:
        pass
    return rss_kb, open_fds, child_procs


def count_session_entries(session_dir: Path) -> int:
    total = 0
    if not session_dir.exists():
        return 0
    for root, _, files in os.walk(session_dir):
        for fname in files:
            if not fname.endswith(".jsonl"):
                continue
            try:
                with (Path(root) / fname).open("r", encoding="utf-8", errors="replace") as fh:
                    for line in fh:
                        if line.strip():
                            total += 1
            except OSError:
                pass
    return total


def session_bytes(session_dir: Path) -> int:
    total = 0
    if not session_dir.exists():
        return 0
    for root, _, files in os.walk(session_dir):
        for fname in files:
            try:
                total += (Path(root) / fname).stat().st_size
            except OSError:
                pass
    return total


def summarize(values: list[float]) -> dict[str, float]:
    if not values:
        return {"first": 0.0, "median": 0.0, "last": 0.0, "min": 0.0, "max": 0.0}
    return {
        "first": float(values[0]),
        "median": float(statistics.median(values)),
        "last": float(values[-1]),
        "min": float(min(values)),
        "max": float(max(values)),
    }


server = socketserver.TCPServer(("127.0.0.1", 0), DeterministicOpenAIHandler)
port = server.server_address[1]
srv_thread = threading.Thread(target=server.serve_forever, daemon=True)
srv_thread.start()

series_path = artifact_dir / "binary_series.jsonl"
stderr_path = artifact_dir / "binary_stderr.log"
summary_path = artifact_dir / "binary_summary.json"
samples: list[dict] = []
warnings_and_errors: list[str] = []
warnings_lock = threading.Lock()
leaked_handles: list = []
injected_unbounded_entries = 0
stopped_by_max_secs = False
failed_early = False
completed_turns = 0
proc_rc: int | None = None
started_monotonic = time.monotonic()

def record_warning(msg: str) -> None:
    with warnings_lock:
        warnings_and_errors.append(msg)

try:
    with tempfile.TemporaryDirectory(prefix="pi-session-soak-") as tmp:
        tmp_path = Path(tmp)
        agent_dir = tmp_path / "agent"
        sessions_dir = tmp_path / "sessions"
        agent_dir.mkdir(parents=True, exist_ok=True)
        sessions_dir.mkdir(parents=True, exist_ok=True)

        models_cfg = {
            "providers": {
                "soak-stub": {
                    "baseUrl": f"http://127.0.0.1:{port}/v1",
                    "api": "openai-completions",
                    "apiKey": "SOAK_STUB_API_KEY",
                    "models": [{
                        "id": "soak-stub-model",
                        "name": "Soak Stub Model",
                        "contextWindow": 128000,
                        "maxTokens": 4096,
                    }],
                }
            }
        }
        (agent_dir / "models.json").write_text(json.dumps(models_cfg, indent=2), encoding="utf-8")

        env = os.environ.copy()
        env["PI_CODING_AGENT_DIR"] = str(agent_dir)
        env["SOAK_STUB_API_KEY"] = "soak-local-deterministic-key"
        env["NO_COLOR"] = "1"
        env["PI_TELEMETRY"] = "0"

        proc = subprocess.Popen(
            [
                pi_bin,
                "--mode", "rpc",
                "--approval-mode", "yolo",
                "--provider", "soak-stub",
                "--model", "soak-stub-model",
                "--session-dir", str(sessions_dir),
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-themes",
            ],
            cwd=str(tmp_path),
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        assert proc.stdin is not None and proc.stdout is not None and proc.stderr is not None

        stdout_queue: queue.Queue[str | None] = queue.Queue()

        def pump_stdout() -> None:
            assert proc.stdout is not None
            try:
                for line in proc.stdout:
                    stdout_queue.put(line)
            finally:
                stdout_queue.put(None)

        stdout_thread = threading.Thread(target=pump_stdout, daemon=True)
        stdout_thread.start()

        def read_rpc_line(timeout_s: float = 30.0) -> str | None:
            try:
                return stdout_queue.get(timeout=timeout_s)
            except queue.Empty:
                return None

        def drain_stderr() -> None:
            assert proc.stderr is not None
            with stderr_path.open("a", encoding="utf-8") as fh:
                for line in proc.stderr:
                    fh.write(line)
                    fh.flush()
                    stripped = line.strip()
                    if stripped and any(tok in stripped.lower() for tok in ("warn", "error", "panic", "fatal")):
                        record_warning(stripped)

        stderr_thread = threading.Thread(target=drain_stderr, daemon=True)
        stderr_thread.start()

        def rpc_call(payload: dict, wait_event_type: str | None = None) -> tuple[bool, float]:
            t0 = time.monotonic()
            proc.stdin.write(json.dumps(payload) + "\n")
            proc.stdin.flush()
            got_response = False
            got_event = wait_event_type is None
            while not (got_response and got_event):
                line = read_rpc_line(30.0)
                if not line:
                    record_warning(f"RPC timeout/EOF waiting for {payload.get('id')}")
                    return False, (time.monotonic() - t0) * 1000.0
                try:
                    msg = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if msg.get("type") == "response" and msg.get("id") == payload.get("id"):
                    if not msg.get("success", False):
                        record_warning(f"rpc response failed: {msg}")
                        return False, (time.monotonic() - t0) * 1000.0
                    got_response = True
                elif wait_event_type and msg.get("type") == wait_event_type:
                    got_event = True
            return True, (time.monotonic() - t0) * 1000.0

        def wait_rpc_idle(turn_idx: int, timeout_s: float = 10.0) -> bool:
            deadline = time.monotonic() + timeout_s
            probe = 0
            while time.monotonic() < deadline:
                probe += 1
                req_id = f"idle-{turn_idx}-{probe}"
                proc.stdin.write(json.dumps({"id": req_id, "type": "get_state"}) + "\n")
                proc.stdin.flush()
                while True:
                    remaining = max(0.2, deadline - time.monotonic())
                    line = read_rpc_line(remaining)
                    if not line:
                        record_warning(f"turn {turn_idx}: timeout/EOF polling get_state")
                        return False
                    try:
                        msg = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if msg.get("type") == "response" and msg.get("id") == req_id:
                        data = msg.get("data") or {}
                        if not data.get("isStreaming", False) and not data.get("isCompacting", False):
                            return True
                        break
                time.sleep(0.01)
            record_warning(f"turn {turn_idx}: RPC did not return to idle within {timeout_s}s")
            return False

        with series_path.open("w", encoding="utf-8") as series_fh:
            for turn in range(turns):
                if max_secs > 0 and (time.monotonic() - started_monotonic) >= max_secs:
                    stopped_by_max_secs = True
                    break
                ok, latency_ms = rpc_call(
                    {"id": f"turn-{turn}", "type": "prompt", "message": f"Soak binary turn {turn}"},
                    wait_event_type="agent_end",
                )
                if not ok or not wait_rpc_idle(turn):
                    failed_early = True
                    break
                completed_turns += 1

                if mutation_fault == "leak_fd":
                    for i in range(4):
                        leaked_handles.append(open(tmp_path / f"leak-{turn}-{i}.tmp", "w", encoding="utf-8"))
                elif mutation_fault == "unbounded_vec":
                    # Harness sensitivity probe: inject +24 synthetic session entries per turn.
                    injected_unbounded_entries += 24

                if turn == 0 or (turn + 1) % sample_interval == 0 or turn + 1 == turns:
                    rss_kb, open_fds, child_procs = proc_metrics(proc.pid)
                    if mutation_fault == "leak_fd":
                        # Driver-level sensitivity check: add harness-held leaked FDs.
                        open_fds += len(leaked_handles)
                    real_entries = count_session_entries(sessions_dir)
                    observed_entries = real_entries + injected_unbounded_entries
                    sample = {
                        "schema": "pi.soak.sample.v1",
                        "timestamp": datetime.now(timezone.utc).isoformat(),
                        "turn_index": turn,
                        "elapsed_ms": int((time.monotonic() - started_monotonic) * 1000),
                        "rss_kb": rss_kb,
                        "open_fds": open_fds,
                        "child_procs": child_procs,
                        "session_entries": observed_entries,
                        "session_index_bytes": session_bytes(sessions_dir),
                        "hostcall_queue_depth": 0,
                        "turn_latency_ms": round(latency_ms, 2),
                    }
                    samples.append(sample)
                    series_fh.write(json.dumps(sample) + "\n")
                    series_fh.flush()

        proc.stdin.close()
        try:
            proc_rc = proc.wait(timeout=15)
            if proc_rc != 0:
                record_warning(f"pi binary exited with non-zero code {proc_rc}")
        except subprocess.TimeoutExpired:
            proc.kill()
            proc_rc = proc.wait(timeout=5)
            record_warning("pi binary did not exit cleanly after stdin close")
        stdout_thread.join(timeout=5)
        stderr_thread.join(timeout=5)
finally:
    for fh in leaked_handles:
        try:
            fh.close()
        except OSError:
            pass
    server.shutdown()
    server.server_close()

fd_vals = [float(s["open_fds"]) for s in samples]
rss_vals = [float(s["rss_kb"]) for s in samples]
child_vals = [float(s["child_procs"]) for s in samples]
lat_vals = [float(s["turn_latency_ms"]) for s in samples]
bytes_vals = [float(s["session_index_bytes"]) for s in samples]
entries_vals = [float(s["session_entries"]) for s in samples]

fd_growth = (fd_vals[-1] - fd_vals[0]) if len(fd_vals) >= 2 else 0.0
child_growth = (child_vals[-1] - child_vals[0]) if len(child_vals) >= 2 else 0.0
rss_growth = (rss_vals[-1] - rss_vals[0]) if len(rss_vals) >= 2 else 0.0
final_entries = int(entries_vals[-1]) if entries_vals else 0
entries_per_turn = final_entries / max(1, completed_turns)

aborted_early = stopped_by_max_secs or failed_early
turns_ok = completed_turns > 0 and not failed_early and (completed_turns == turns or stopped_by_max_secs)

verdicts = [
    {
        "metric": "open_fds",
        "status": "pass" if fd_growth <= 16 else "fail",
        "threshold": "growth <= 16 (provisional)",
        "detail": f"growth={fd_growth:.0f}",
    },
    {
        "metric": "child_procs",
        "status": "pass" if child_growth <= 2 else "fail",
        "threshold": "growth <= 2 (provisional)",
        "detail": f"growth={child_growth:.0f}",
    },
    {
        "metric": "rss_kb",
        "status": "pass" if rss_growth <= 262144 else "fail",
        "threshold": "growth <= 262144 KB (provisional)",
        "detail": f"growth={rss_growth:.0f} KB",
    },
    {
        "metric": "session_entries",
        "status": "pass" if entries_per_turn <= 12.0 else "fail",
        "threshold": "entries_per_turn <= 12.0 (provisional)",
        "detail": f"entries={final_entries}, per_turn={entries_per_turn:.2f}",
    },
    {
        "metric": "completed_turns",
        "status": "pass" if turns_ok else "fail",
        "threshold": "completed_turns == requested (or > 0 on --max-secs) without turn failure",
        "detail": f"completed={completed_turns}/{turns}, stopped_by_max_secs={stopped_by_max_secs}, failed_early={failed_early}",
    },
    {
        "metric": "process_exit",
        "status": "pass" if proc_rc == 0 else "fail",
        "threshold": "exit_code == 0",
        "detail": f"exit_code={proc_rc}",
    },
]

summary_table = {
    "rss_kb": summarize(rss_vals),
    "open_fds": summarize(fd_vals),
    "child_procs": summarize(child_vals),
    "session_entries": summarize(entries_vals),
    "session_index_bytes": summarize(bytes_vals),
    "turn_latency_ms": summarize(lat_vals),
}
overall_pass = all(v["status"] == "pass" for v in verdicts)
summary = {
    "schema": "pi.soak.session_duration.binary.v1",
    "scenario_parameters": {
        "binary_turns": turns,
        "sample_interval": sample_interval,
        "max_secs": max_secs,
        "mutation_fault": mutation_fault,
        "pi_bin": pi_bin,
    },
    "completed_turns": completed_turns,
    "requested_turns": turns,
    "aborted_early": aborted_early,
    "stopped_by_max_secs": stopped_by_max_secs,
    "failed_early": failed_early,
    "mutation_fault": mutation_fault,
    "provisional_thresholds": True,
    "warnings_and_errors": warnings_and_errors,
    "verdicts": verdicts,
    "summary_table": summary_table,
    "overall_status": "pass" if overall_pass else "fail",
}
summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

print("\n| Metric | First | Median | Last | Verdict |")
print("|---|---:|---:|---:|---|")
verdict_map = {v["metric"]: v["status"] for v in verdicts}
for metric, stats in summary_table.items():
    print(f"| {metric} | {stats['first']:.1f} | {stats['median']:.1f} | {stats['last']:.1f} | {verdict_map.get(metric, 'info')} |")
print(f"\n[session-soak] Binary phase overall_status={summary['overall_status']} (artifacts: {artifact_dir})")
sys.exit(0 if overall_pass else 1)
PY
    then
        OVERALL_RC=1
    fi
fi

if [[ "$RUN_RUST" -eq 1 ]]; then
    echo "[session-soak] Phase 2: Parameterized Rust duration soak (soak_session_duration_extended)"
    USE_RCH=0
    if [[ "$CARGO_RUNNER" == "rch" ]] || { [[ "$CARGO_RUNNER" == "auto" ]] && command -v rch >/dev/null 2>&1 && rch check --quiet >/dev/null 2>&1; }; then
        USE_RCH=1
    fi
    if [[ "$USE_RCH" -eq 1 ]]; then
        # Default to -j 2 on remote RCH workers (ignoring DSR's ambient local
        # CARGO_BUILD_JOBS=16, which inflates RCH disk-budget admission to ~63 GiB
        # and exhausts worker slot caps). Override explicitly via PI_SOAK_CARGO_BUILD_JOBS.
        JOBS="${PI_SOAK_CARGO_BUILD_JOBS:-2}"
        if [[ ! "$JOBS" =~ ^[0-9]+$ ]] || [[ "$JOBS" -eq 0 ]]; then
            echo "Invalid PI_SOAK_CARGO_BUILD_JOBS=$JOBS (expected positive integer)" >&2
            exit 2
        fi
        if ! RCH_BUILD_TIMEOUT_SEC="${RCH_BUILD_TIMEOUT_SEC:-3600}" \
             RCH_TEST_TIMEOUT_SEC="${RCH_TEST_TIMEOUT_SEC:-3600}" \
             CARGO_BUILD_JOBS="$JOBS" \
             rch exec --base HEAD --clean-overlay --no-overlay -- \
             env \
             TMPDIR=/tmp \
             CARGO_INCREMENTAL=0 \
             CARGO_PROFILE_DEV_DEBUG=0 \
             CARGO_PROFILE_DEV_STRIP=symbols \
             CARGO_PROFILE_TEST_DEBUG=0 \
             CARGO_PROFILE_TEST_STRIP=symbols \
             PI_SOAK_TURNS="$TURNS" \
             PI_SOAK_MAX_SECS="$MAX_SECS" \
             PI_SOAK_SAMPLE_INTERVAL="$SAMPLE_INTERVAL" \
             PI_SOAK_MUTATION_FAULT="$MUTATION_FAULT" \
             cargo test -j "$JOBS" --locked --test e2e_soak_stability soak_session_duration_extended -- --ignored --nocapture \
             2>&1 | tee "$ARTIFACT_DIR/rust_soak.log"; then
            OVERALL_RC=1
        fi
    else
        if ! PI_SOAK_TURNS="$TURNS" \
             PI_SOAK_MAX_SECS="$MAX_SECS" \
             PI_SOAK_SAMPLE_INTERVAL="$SAMPLE_INTERVAL" \
             PI_SOAK_MUTATION_FAULT="$MUTATION_FAULT" \
             PI_SOAK_ARTIFACT_DIR="$ARTIFACT_DIR" \
             cargo test --locked --test e2e_soak_stability soak_session_duration_extended -- --ignored --nocapture \
             2>&1 | tee "$ARTIFACT_DIR/rust_soak.log"; then
            OVERALL_RC=1
        fi
    fi
fi

exit "$OVERALL_RC"
