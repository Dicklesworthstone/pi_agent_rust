#!/usr/bin/env bash
# scripts/e2e/run_tool_concurrency_soak.sh — Explicit sustained multi-tool concurrency soak lane (GH #213, bd-brx7l).
#
# Runs outside the ordinary DSR quality lane so sustained multi-tool concurrency
# pressure and hostcall-queue overflow tests never create cross-test noise in
# per-commit checks. Drives BOTH:
#   1. The real `pi` binary against a deterministic local loopback OpenAI-compatible
#      SSE stub server emitting multi-tool waves (compatible read-only calls
#      interleaved with write/edit/bash barriers), zero credentials required.
#   2. Optionally (default: enabled unless --binary-only), the parameterized Rust
#      concurrency soak (`soak_sustained_tool_concurrency_extended`) exercising
#      tool-effect batching decisions, mid-batch steering, and real HostcallRequestQueue
#      fast-ring (256) vs overflow (2048) backpressure.
#
# Usage:
#   scripts/e2e/run_tool_concurrency_soak.sh [options]
#
# Options:
#   --waves N             Number of concurrency waves (default: 24; binary waves default: 6)
#   --binary-waves N      Number of multi-tool waves through the real `pi` binary (default: 6)
#   --parallel-reads R    Compatible parallel read-only tool calls per wave (default: 8)
#   --hostcall-burst B    Hostcall burst size per wave to cross fast ring -> overflow (default: 320)
#   --max-secs S          Optional wall-clock budget cap in seconds (default: unset)
#   --binary-only         Run only the real `pi` binary multi-tool concurrency phase
#   --rust-only           Run only the parameterized Rust concurrency soak test
#   --pi-bin PATH         Explicit path to `pi` binary (default: auto-detect)
#   --artifact-dir DIR    Output directory (default: tests/e2e_results/tool-concurrency-soak/<run_id>)
#   --dry-run             Validate configuration & print execution plan without running
#   --help, -h            Show this help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$PROJECT_ROOT"

WAVES="${PI_CONCURRENCY_WAVES:-24}"
BINARY_WAVES="${PI_CONCURRENCY_BINARY_WAVES:-6}"
PARALLEL_READS="${PI_CONCURRENCY_PARALLEL_READS:-8}"
HOSTCALL_BURST="${PI_CONCURRENCY_HOSTCALL_BURST:-320}"
MAX_SECS="${PI_SOAK_MAX_SECS:-}"
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
        --waves)
            require_arg "$#" "$1"
            WAVES="$2"
            shift 2
            ;;
        --binary-waves)
            require_arg "$#" "$1"
            BINARY_WAVES="$2"
            shift 2
            ;;
        --parallel-reads)
            require_arg "$#" "$1"
            PARALLEL_READS="$2"
            shift 2
            ;;
        --hostcall-burst)
            require_arg "$#" "$1"
            HOSTCALL_BURST="$2"
            shift 2
            ;;
        --max-secs)
            require_arg "$#" "$1"
            MAX_SECS="$2"
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

for num_var in WAVES BINARY_WAVES PARALLEL_READS HOSTCALL_BURST; do
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
    ARTIFACT_DIR="$PROJECT_ROOT/tests/e2e_results/tool-concurrency-soak/$RUN_ID"
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
    python3 - "$RUN_ID" "$ARTIFACT_DIR" "$PI_BIN" "$WAVES" "$BINARY_WAVES" "$PARALLEL_READS" "$HOSTCALL_BURST" "$MAX_SECS" "$RUN_BINARY" "$RUN_RUST" <<'PY'
import json, sys
run_id, art_dir, pi_bin, waves, bin_waves, reads, burst, max_secs, run_bin, run_rust = sys.argv[1:]
print(json.dumps({
    "schema": "pi.soak.tool_concurrency.plan.v1",
    "dry_run": True,
    "run_id": run_id,
    "artifact_dir": art_dir,
    "pi_bin": pi_bin or None,
    "waves": int(waves),
    "binary_waves": int(bin_waves),
    "parallel_reads": int(reads),
    "hostcall_burst": int(burst),
    "max_secs": int(max_secs) if max_secs else None,
    "run_binary_phase": run_bin == "1",
    "run_rust_phase": run_rust == "1",
}, indent=2))
PY
    exit 0
fi

mkdir -p "$ARTIFACT_DIR"
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd -P)"
GIT_SHA="$(git rev-parse HEAD 2>/dev/null || echo unknown)"

python3 - "$ARTIFACT_DIR/params.json" "$RUN_ID" "$GIT_SHA" "$PI_BIN" "$WAVES" "$BINARY_WAVES" "$PARALLEL_READS" "$HOSTCALL_BURST" "$MAX_SECS" "$RUN_BINARY" "$RUN_RUST" <<'PY'
import json, sys
from datetime import datetime, timezone
out, run_id, sha, pi_bin, waves, bin_waves, reads, burst, max_secs, run_bin, run_rust = sys.argv[1:]
payload = {
    "schema": "pi.soak.tool_concurrency.params.v1",
    "run_id": run_id,
    "started_at": datetime.now(timezone.utc).isoformat(),
    "git_sha": sha,
    "pi_bin": pi_bin or None,
    "waves": int(waves),
    "binary_waves": int(bin_waves),
    "parallel_reads": int(reads),
    "hostcall_burst": int(burst),
    "max_secs": int(max_secs) if max_secs else None,
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
        echo "[tool-concurrency-soak] ERROR: no executable pi binary found (pass --pi-bin PATH)" >&2
        exit 2
    fi
    echo "[tool-concurrency-soak] Phase 1: Real pi binary multi-tool concurrency soak ($BINARY_WAVES waves x $PARALLEL_READS reads + barriers, pi=$PI_BIN)"
    if ! python3 - "$PI_BIN" "$ARTIFACT_DIR" "$BINARY_WAVES" "$PARALLEL_READS" "${MAX_SECS:-0}" <<'PY'
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
waves = int(sys.argv[3])
parallel_reads = int(sys.argv[4])
max_secs = int(sys.argv[5])

state_lock = threading.Lock()
call_index = 0
wave_workspace: Path | None = None


class MultiToolOpenAIHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        return

    def do_POST(self):
        global call_index
        length = int(self.headers.get("Content-Length", "0"))
        _ = self.rfile.read(length) if length > 0 else b""
        with state_lock:
            idx = call_index
            call_index += 1
            ws = wave_workspace

        assert ws is not None
        wave_num = idx // 2
        if idx % 2 == 0:
            # Emit N compatible read calls + 1 write barrier + 1 trailing read call in one turn.
            tool_calls = []
            for r in range(parallel_reads):
                target = ws / f"fixture_{r % 4}.txt"
                tool_calls.append({
                    "index": r,
                    "id": f"call_w{wave_num}_r{r}",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": json.dumps({"path": str(target)}),
                    },
                })
            barrier_idx = parallel_reads
            barrier_path = ws / f"wave_out_{wave_num}.txt"
            tool_calls.append({
                "index": barrier_idx,
                "id": f"call_w{wave_num}_write",
                "type": "function",
                "function": {
                    "name": "write",
                    "arguments": json.dumps({
                        "path": str(barrier_path),
                        "content": f"wave-{wave_num}-barrier-ok\n",
                    }),
                },
            })
            tool_calls.append({
                "index": barrier_idx + 1,
                "id": f"call_w{wave_num}_post_read",
                "type": "function",
                "function": {
                    "name": "read",
                    "arguments": json.dumps({"path": str(barrier_path)}),
                },
            })
            chunks = [{
                "id": f"chatcmpl-conc-{idx}",
                "object": "chat.completion.chunk",
                "created": int(time.time()),
                "model": "conc-stub-model",
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "tool_calls": tool_calls},
                    "finish_reason": None,
                }],
            }, {
                "id": f"chatcmpl-conc-{idx}",
                "object": "chat.completion.chunk",
                "created": int(time.time()),
                "model": "conc-stub-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 32, "completion_tokens": 48, "total_tokens": 80},
            }]
        else:
            chunks = [{
                "id": f"chatcmpl-conc-{idx}",
                "object": "chat.completion.chunk",
                "created": int(time.time()),
                "model": "conc-stub-model",
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": f"wave-{wave_num}-complete"},
                    "finish_reason": None,
                }],
            }, {
                "id": f"chatcmpl-conc-{idx}",
                "object": "chat.completion.chunk",
                "created": int(time.time()),
                "model": "conc-stub-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 48, "completion_tokens": 12, "total_tokens": 60},
            }]

        body = "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"
        encoded = body.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(encoded)


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


server = socketserver.TCPServer(("127.0.0.1", 0), MultiToolOpenAIHandler)
port = server.server_address[1]
threading.Thread(target=server.serve_forever, daemon=True).start()

series_path = artifact_dir / "binary_series.jsonl"
stderr_path = artifact_dir / "binary_stderr.log"
summary_path = artifact_dir / "binary_summary.json"
samples: list[dict] = []
warnings_and_errors: list[str] = []
warnings_lock = threading.Lock()
completed_waves = 0
stopped_by_max_secs = False
failed_early = False
proc_rc: int | None = None
started_monotonic = time.monotonic()

def record_warning(msg: str) -> None:
    with warnings_lock:
        warnings_and_errors.append(msg)

try:
    with tempfile.TemporaryDirectory(prefix="pi-conc-soak-") as tmp:
        tmp_path = Path(tmp)
        wave_workspace = tmp_path
        for i in range(4):
            (tmp_path / f"fixture_{i}.txt").write_text(f"fixture-{i}-line-1\nfixture-{i}-line-2\n", encoding="utf-8")

        agent_dir = tmp_path / "agent"
        sessions_dir = tmp_path / "sessions"
        agent_dir.mkdir(parents=True, exist_ok=True)
        sessions_dir.mkdir(parents=True, exist_ok=True)

        models_cfg = {
            "providers": {
                "conc-stub": {
                    "baseUrl": f"http://127.0.0.1:{port}/v1",
                    "api": "openai-completions",
                    "apiKey": "CONC_STUB_API_KEY",
                    "models": [{
                        "id": "conc-stub-model",
                        "name": "Concurrency Stub Model",
                        "contextWindow": 128000,
                        "maxTokens": 4096,
                    }],
                }
            }
        }
        (agent_dir / "models.json").write_text(json.dumps(models_cfg, indent=2), encoding="utf-8")

        env = os.environ.copy()
        env["PI_CODING_AGENT_DIR"] = str(agent_dir)
        env["CONC_STUB_API_KEY"] = "conc-local-deterministic-key"
        env["NO_COLOR"] = "1"
        env["PI_TELEMETRY"] = "0"

        proc = subprocess.Popen(
            [
                pi_bin,
                "--mode", "rpc",
                "--approval-mode", "yolo",
                "--provider", "conc-stub",
                "--model", "conc-stub-model",
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

        def wait_rpc_idle(wave_idx: int, timeout_s: float = 10.0) -> bool:
            deadline = time.monotonic() + timeout_s
            probe = 0
            while time.monotonic() < deadline:
                probe += 1
                req_id = f"idle-{wave_idx}-{probe}"
                proc.stdin.write(json.dumps({"id": req_id, "type": "get_state"}) + "\n")
                proc.stdin.flush()
                while True:
                    remaining = max(0.2, deadline - time.monotonic())
                    line = read_rpc_line(remaining)
                    if not line:
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
            record_warning(f"wave {wave_idx}: RPC did not return to idle within {timeout_s}s")
            return False

        with series_path.open("w", encoding="utf-8") as series_fh:
            for wave in range(waves):
                if max_secs > 0 and (time.monotonic() - started_monotonic) >= max_secs:
                    stopped_by_max_secs = True
                    break
                t0 = time.monotonic()
                req = {"id": f"wave-{wave}", "type": "prompt", "message": f"Execute concurrency wave {wave}"}
                proc.stdin.write(json.dumps(req) + "\n")
                proc.stdin.flush()

                tool_starts = 0
                tool_ends = 0
                in_flight = 0
                peak_in_flight = 0
                post_read_saw_barrier = False
                got_response = False
                got_end = False
                while not (got_response and got_end):
                    line = read_rpc_line(30.0)
                    if line is None:
                        record_warning(f"timeout waiting for RPC output on wave {wave}")
                        break
                    if not line:
                        record_warning(f"unexpected EOF on wave {wave}")
                        break
                    try:
                        msg = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    mtype = msg.get("type")
                    if mtype == "response" and msg.get("id") == req["id"]:
                        if not msg.get("success", False):
                            record_warning(f"wave {wave} rpc failed: {msg}")
                            break
                        got_response = True
                    elif mtype == "tool_execution_start":
                        tool_starts += 1
                        in_flight += 1
                        peak_in_flight = max(peak_in_flight, in_flight)
                    elif mtype == "tool_execution_end":
                        tool_ends += 1
                        in_flight = max(0, in_flight - 1)
                        if msg.get("toolCallId") == f"call_w{wave}_post_read":
                            rendered = json.dumps(msg.get("result", {}))
                            if f"wave-{wave}-barrier-ok" in rendered and not msg.get("isError", False):
                                post_read_saw_barrier = True
                    elif mtype == "agent_end":
                        got_end = True

                idle_ok = wait_rpc_idle(wave) if (got_response and got_end) else False
                elapsed_ms = (time.monotonic() - t0) * 1000.0
                expected_file = tmp_path / f"wave_out_{wave}.txt"
                barrier_ok = (
                    expected_file.exists()
                    and expected_file.read_text(encoding="utf-8") == f"wave-{wave}-barrier-ok\n"
                    and post_read_saw_barrier
                )
                if not barrier_ok:
                    record_warning(f"wave {wave} barrier file/post-read ordering check failed (post_read_ok={post_read_saw_barrier})")
                if got_response and got_end and idle_ok:
                    completed_waves += 1
                else:
                    failed_early = True
                    break
                sample = {
                    "schema": "pi.soak.concurrency_sample.v1",
                    "timestamp": datetime.now(timezone.utc).isoformat(),
                    "wave_index": wave,
                    "tool_starts": tool_starts,
                    "tool_ends": tool_ends,
                    "peak_in_flight": peak_in_flight,
                    "expected_tool_calls": parallel_reads + 2,
                    "effect_batching": {
                        "parallel_read_calls": parallel_reads,
                        "barrier_write_calls": 1,
                        "post_barrier_read_calls": 1,
                        "expected_batches": 3,
                    },
                    "barrier_integrity_ok": barrier_ok,
                    "wave_latency_ms": round(elapsed_ms, 2),
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
    server.shutdown()
    server.server_close()

aborted_early = stopped_by_max_secs or failed_early
balanced = bool(samples) and all(s["tool_starts"] == s["tool_ends"] == s["expected_tool_calls"] for s in samples)
all_barriers_ok = bool(samples) and all(s["barrier_integrity_ok"] for s in samples)
waves_ok = (
    not failed_early
    and (
        (completed_waves > 0 and completed_waves <= waves)
        if stopped_by_max_secs
        else completed_waves == waves
    )
)
verdicts = [
    {
        "metric": "completed_waves",
        "status": "pass" if waves_ok else "fail",
        "threshold": f"completed_waves == {waves}" if not stopped_by_max_secs else "completed_waves > 0 under --max-secs without wave failure",
        "detail": f"completed={completed_waves}/{waves}, stopped_by_max_secs={stopped_by_max_secs}, failed_early={failed_early}",
    },
    {
        "metric": "tool_balance",
        "status": "pass" if balanced else "fail",
        "threshold": "tool_starts == tool_ends == expected_tool_calls per wave",
        "detail": f"balanced={balanced}",
    },
    {
        "metric": "barrier_integrity",
        "status": "pass" if all_barriers_ok else "fail",
        "threshold": "all barrier writes and post-read ordering intact",
        "detail": f"all_barriers_ok={all_barriers_ok}",
    },
    {
        "metric": "process_exit",
        "status": "pass" if proc_rc == 0 else "fail",
        "threshold": "exit_code == 0",
        "detail": f"exit_code={proc_rc}",
    },
]
summary_table = {
    "tool_starts": summarize([float(s["tool_starts"]) for s in samples]),
    "peak_in_flight": summarize([float(s["peak_in_flight"]) for s in samples]),
    "wave_latency_ms": summarize([float(s["wave_latency_ms"]) for s in samples]),
}
overall_pass = all(v["status"] == "pass" for v in verdicts)
summary = {
    "schema": "pi.soak.tool_concurrency.binary.v1",
    "scenario_parameters": {
        "binary_waves": waves,
        "parallel_reads_per_wave": parallel_reads,
        "max_secs": max_secs,
        "pi_bin": pi_bin,
    },
    "hostcall_queue_telemetry": {
        "fast_ring_capacity": 256,
        "overflow_capacity": 2048,
        "binary_phase_extensions_enabled": False,
        "note": "Real HostcallRequestQueue fast-ring vs overflow admission counts are captured by Phase 2 (soak_sustained_tool_concurrency_extended).",
    },
    "completed_waves": completed_waves,
    "requested_waves": waves,
    "aborted_early": aborted_early,
    "stopped_by_max_secs": stopped_by_max_secs,
    "failed_early": failed_early,
    "parallel_reads_per_wave": parallel_reads,
    "warnings_and_errors": warnings_and_errors,
    "verdicts": verdicts,
    "summary_table": summary_table,
    "overall_status": "pass" if overall_pass else "fail",
}
summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

print("\n| Metric | First | Median | Last |")
print("|---|---:|---:|---:|")
for metric, stats in summary_table.items():
    print(f"| {metric} | {stats['first']:.1f} | {stats['median']:.1f} | {stats['last']:.1f} |")
print(f"\n[tool-concurrency-soak] Binary phase overall_status={summary['overall_status']} (artifacts: {artifact_dir})")
sys.exit(0 if overall_pass else 1)
PY
    then
        OVERALL_RC=1
    fi
fi

if [[ "$RUN_RUST" -eq 1 ]]; then
    echo "[tool-concurrency-soak] Phase 2: Parameterized Rust concurrency soak (soak_sustained_tool_concurrency_extended)"
    declare -a RUNNER=(cargo)
    if [[ "$CARGO_RUNNER" == "rch" ]] || { [[ "$CARGO_RUNNER" == "auto" ]] && command -v rch >/dev/null 2>&1 && rch check --quiet >/dev/null 2>&1; }; then
        RUNNER=(rch exec -- cargo)
    fi
    if ! PI_CONCURRENCY_WAVES="$WAVES" \
         PI_CONCURRENCY_PARALLEL_READS="$PARALLEL_READS" \
         PI_CONCURRENCY_HOSTCALL_BURST="$HOSTCALL_BURST" \
         PI_SOAK_MAX_SECS="$MAX_SECS" \
         PI_SOAK_ARTIFACT_DIR="$ARTIFACT_DIR" \
         "${RUNNER[@]}" test --locked --test e2e_soak_stability soak_sustained_tool_concurrency_extended -- --ignored --nocapture; then
        OVERALL_RC=1
    fi
fi

exit "$OVERALL_RC"
