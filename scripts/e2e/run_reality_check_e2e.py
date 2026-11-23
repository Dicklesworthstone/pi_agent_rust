#!/usr/bin/env python3
"""End-to-end live verification gate for the 2026-11-19 Reality-Check Bridge (.10).

Runs live CLI/RPC/exit-code smoke checks against a `pi` binary (using a local
deterministic loopback OpenAI-compatible SSE server so zero network credentials
are needed for offline checks, plus an optional live default-model probe),
alongside repository governance/supply-chain validators, and emits a structured
JSONL verification bundle (`schema: "pi.reality_check.e2e_verification.v1"`).
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import queue
import re
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SCHEMA_ID = "pi.reality_check.e2e_verification.v1"
REPO_ROOT = Path(__file__).resolve().parent.parent.parent
VERSION_SHA_RE = re.compile(r"^pi\s+\d+\.\d+\.\d+\S*\s+\(([0-9a-f]{7,40})\b")


@dataclass
class CheckRecord:
    check_name: str
    command: str
    exit_code: int
    expected_exit_code: int | None
    duration_ms: int
    git_sha: str
    dsr_run_dir: str | None
    verdict: str
    detail: str

    def to_json(self) -> dict[str, Any]:
        return {
            "schema": SCHEMA_ID,
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "check_name": self.check_name,
            "command": self.command,
            "exit_code": self.exit_code,
            "expected_exit_code": self.expected_exit_code,
            "duration_ms": self.duration_ms,
            "git_sha": self.git_sha,
            "dsr_run_dir": self.dsr_run_dir,
            "verdict": self.verdict,
            "detail": self.detail,
        }


def resolve_git_sha() -> str:
    res = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=str(REPO_ROOT),
        capture_output=True,
        text=True,
        check=False,
    )
    return res.stdout.strip() if res.returncode == 0 else "unknown"


def auto_detect_pi_bin() -> str | None:
    for candidate in (
        REPO_ROOT / "target" / "dist" / "pi",
        REPO_ROOT / "target" / "release" / "pi",
        REPO_ROOT / "target" / "debug" / "pi",
    ):
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    which_pi = shutil.which("pi")
    return which_pi


def validate_version_output(stdout: str, expected_git_sha: str | None = None) -> tuple[bool, str]:
    line = stdout.strip().splitlines()[0] if stdout.strip() else ""
    if not line:
        return False, "empty --version output"
    if "unknown" in line.lower():
        return False, f"version prints forbidden 'unknown' commit marker: {line}"
    match = VERSION_SHA_RE.match(line)
    if not match:
        return False, f"version does not contain canonical hex commit SHA: {line}"
    found_sha = match.group(1).lower()
    if expected_git_sha and expected_git_sha != "unknown":
        expected_lower = expected_git_sha.lower()
        if not expected_lower.startswith(found_sha) and not found_sha.startswith(expected_lower):
            return (
                False,
                f"version commit {found_sha} does not match expected HEAD {expected_lower[:12]}: {line}",
            )
    return True, f"verified version line with commit {found_sha}: {line}"


class LoopbackStubProvider:
    """Local deterministic OpenAI-compatible SSE server for offline binary E2E checks."""

    def __init__(self) -> None:
        self.mode = "ok"
        self.turn = 0
        self.requests: list[dict[str, Any]] = []
        self.lock = threading.Lock()
        stub = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, fmt: str, *args: Any) -> None:
                return

            def do_POST(self) -> None:
                length = int(self.headers.get("Content-Length", "0"))
                raw_body = self.rfile.read(length) if length > 0 else b""
                parsed_body: dict[str, Any] = {}
                if raw_body:
                    try:
                        decoded = json.loads(raw_body.decode("utf-8"))
                        if isinstance(decoded, dict):
                            parsed_body = decoded
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        parsed_body = {}
                with stub.lock:
                    mode = stub.mode
                    stub.turn += 1
                    idx = stub.turn
                    stub.requests.append(parsed_body)

                if mode == "http_401":
                    err_body = json.dumps({
                        "error": {
                            "message": "deterministic loopback 401 failure",
                            "type": "authentication_error",
                        }
                    }).encode("utf-8")
                    self.send_response(401)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(err_body)))
                    self.send_header("Connection", "close")
                    self.end_headers()
                    self.wfile.write(err_body)
                    return

                if mode == "bash_tool_once" and idx == 1:
                    chunks = [
                        {
                            "id": f"chatcmpl-e2e-{idx}",
                            "object": "chat.completion.chunk",
                            "created": int(time.time()),
                            "model": "e2e-stub-model",
                            "choices": [{
                                "index": 0,
                                "delta": {
                                    "role": "assistant",
                                    "tool_calls": [{
                                        "index": 0,
                                        "id": "call_bash_approval_probe",
                                        "type": "function",
                                        "function": {
                                            "name": "bash",
                                            "arguments": json.dumps({"command": "echo hi"}),
                                        },
                                    }],
                                },
                                "finish_reason": None,
                            }],
                        },
                        {
                            "id": f"chatcmpl-e2e-{idx}",
                            "object": "chat.completion.chunk",
                            "created": int(time.time()),
                            "model": "e2e-stub-model",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                            "usage": {"prompt_tokens": 20, "completion_tokens": 12, "total_tokens": 32},
                        },
                    ]
                else:
                    prompt_tokens = 1000 if mode == "expensive" else 16
                    completion_tokens = 500 if mode == "expensive" else 8
                    chunks = [
                        {
                            "id": f"chatcmpl-e2e-{idx}",
                            "object": "chat.completion.chunk",
                            "created": int(time.time()),
                            "model": "e2e-stub-model",
                            "choices": [{
                                "index": 0,
                                "delta": {
                                    "role": "assistant",
                                    "content": f"OK loopback turn {idx}",
                                },
                                "finish_reason": None,
                            }],
                        },
                        {
                            "id": f"chatcmpl-e2e-{idx}",
                            "object": "chat.completion.chunk",
                            "created": int(time.time()),
                            "model": "e2e-stub-model",
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                            "usage": {
                                "prompt_tokens": prompt_tokens,
                                "completion_tokens": completion_tokens,
                                "total_tokens": prompt_tokens + completion_tokens,
                            },
                        },
                    ]

                body = (
                    "".join(f"data: {json.dumps(c)}\n\n" for c in chunks)
                    + "data: [DONE]\n\n"
                ).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(body)

        self.server = socketserver.TCPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_address[1]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def set_mode(self, mode: str) -> None:
        with self.lock:
            self.mode = mode
            self.turn = 0
            self.requests.clear()

    def last_request_messages(self) -> list[Any]:
        with self.lock:
            if not self.requests:
                return []
            msgs = self.requests[-1].get("messages")
            return msgs if isinstance(msgs, list) else []

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


def write_stub_agent_dir(base: Path, port: int) -> tuple[Path, dict[str, str]]:
    agent_dir = base / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    models_cfg = {
        "providers": {
            "e2e-stub": {
                "baseUrl": f"http://127.0.0.1:{port}/v1",
                "api": "openai-completions",
                "apiKey": "E2E_STUB_API_KEY",
                "models": [{
                    "id": "e2e-stub-model",
                    "name": "E2E Stub Model",
                    "contextWindow": 128000,
                    "maxTokens": 4096,
                    "cost": {
                        "input": 10.0,
                        "output": 30.0,
                        "cacheRead": 1.0,
                        "cacheWrite": 1.0,
                    },
                }],
            }
        }
    }
    (agent_dir / "models.json").write_text(json.dumps(models_cfg, indent=2), encoding="utf-8")
    env = os.environ.copy()
    env["PI_CODING_AGENT_DIR"] = str(agent_dir)
    env["E2E_STUB_API_KEY"] = "e2e-stub-local-key"
    env["NO_COLOR"] = "1"
    env["PI_TELEMETRY"] = "0"
    return agent_dir, env


def run_cmd(
    argv: list[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
    timeout: int = 120,
) -> tuple[int, str, str, int]:
    t0 = time.monotonic()
    res = subprocess.run(
        argv,
        cwd=str(cwd),
        env=env,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    elapsed_ms = int((time.monotonic() - t0) * 1000)
    return res.returncode, res.stdout, res.stderr, elapsed_ms


def run_rpc_smoke(
    pi_bin: str,
    cwd: Path,
    env: dict[str, str],
    stub: LoopbackStubProvider,
) -> tuple[int, int, str]:
    stub.set_mode("ok")
    t0 = time.monotonic()
    proc = subprocess.Popen(
        [
            pi_bin,
            "--mode", "rpc",
            "--no-session",
            "--approval-mode", "yolo",
            "--provider", "e2e-stub",
            "--model", "e2e-stub-model",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
        ],
        cwd=str(cwd),
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    assert proc.stdin is not None and proc.stdout is not None and proc.stderr is not None

    # Use background reader threads for both stdout and stderr so buffered lines
    # inside Python's TextIOWrapper never stall behind select.select() and a noisy
    # stderr stream can never fill the 64 KiB OS pipe buffer and deadlock pi.
    stdout_queue: queue.Queue[str | None] = queue.Queue()
    stderr_chunks: list[str] = []

    def pump_stdout() -> None:
        assert proc.stdout is not None
        try:
            for line in proc.stdout:
                stdout_queue.put(line)
        finally:
            stdout_queue.put(None)

    def pump_stderr() -> None:
        assert proc.stderr is not None
        for line in proc.stderr:
            stderr_chunks.append(line)
            if len(stderr_chunks) > 256:
                del stderr_chunks[:-256]

    reader_thread = threading.Thread(target=pump_stdout, daemon=True)
    stderr_thread = threading.Thread(target=pump_stderr, daemon=True)
    reader_thread.start()
    stderr_thread.start()

    def read_line(timeout_s: float = 20.0) -> str | None:
        try:
            return stdout_queue.get(timeout=timeout_s)
        except queue.Empty:
            return None

    def send_and_wait(payload: dict[str, Any], wait_event: str | None = None) -> tuple[bool, dict[str, Any] | None]:
        proc.stdin.write(json.dumps(payload) + "\n")
        proc.stdin.flush()
        resp: dict[str, Any] | None = None
        got_event = wait_event is None
        while resp is None or not got_event:
            line = read_line(20.0)
            if not line:
                return False, resp
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("type") == "response" and msg.get("id") == payload.get("id"):
                resp = msg
                if not msg.get("success", False):
                    return False, resp
            elif wait_event and msg.get("type") == wait_event:
                got_event = True
        return True, resp

    def wait_idle(tag: str) -> tuple[bool, dict[str, Any]]:
        deadline = time.monotonic() + 10.0
        n = 0
        while time.monotonic() < deadline:
            n += 1
            ok, resp = send_and_wait({"id": f"idle-{tag}-{n}", "type": "get_state"})
            if not ok or resp is None:
                return False, {}
            data = resp.get("data") or {}
            if not data.get("isStreaming", False) and not data.get("isCompacting", False):
                return True, data
            time.sleep(0.01)
        return False, {}

    try:
        def is_plain_int(val: Any) -> bool:
            return isinstance(val, int) and not isinstance(val, bool)

        def fetch_stats(tag: str) -> tuple[bool, dict[str, Any]]:
            ok_st, st_resp = send_and_wait({"id": f"stats-{tag}", "type": "get_session_stats"})
            if not ok_st or st_resp is None:
                return False, {}
            return True, st_resp.get("data") or {}

        ok_state, _ = send_and_wait({"id": "st-1", "type": "get_state"})
        if not ok_state:
            return 1, int((time.monotonic() - t0) * 1000), "RPC get_state failed"

        ok_p1, _ = send_and_wait(
            {"id": "p-1", "type": "prompt", "message": "First turn before checkpoint"},
            wait_event="agent_end",
        )
        idle_p1, state_p1 = wait_idle("p1")
        ok_stats_p1, stats_p1 = fetch_stats("p1")
        if not ok_p1 or not idle_p1 or not ok_stats_p1:
            return 1, int((time.monotonic() - t0) * 1000), "RPC prompt 1 failed"
        count_at_cp = state_p1.get("messageCount")
        active_at_cp = stats_p1.get("totalMessages")

        ok_cp, cp_resp = send_and_wait({"id": "cp-1", "type": "checkpoint", "name": "alpha"})
        if not ok_cp:
            return 1, int((time.monotonic() - t0) * 1000), f"RPC checkpoint failed: {cp_resp}"

        ok_p2, _ = send_and_wait(
            {"id": "p-2", "type": "prompt", "message": "Second turn to rewind"},
            wait_event="agent_end",
        )
        idle_p2, state_p2 = wait_idle("p2")
        ok_stats_p2, stats_p2 = fetch_stats("p2")
        if not ok_p2 or not idle_p2 or not ok_stats_p2:
            return 1, int((time.monotonic() - t0) * 1000), "RPC prompt 2 failed"
        count_after_p2 = state_p2.get("messageCount")
        active_after_p2 = stats_p2.get("totalMessages")

        ok_rw, rw_resp = send_and_wait({"id": "rw-1", "type": "rewind", "name": "alpha"})
        if not ok_rw or rw_resp is None:
            return 1, int((time.monotonic() - t0) * 1000), f"RPC rewind failed: {rw_resp}"
        rw_data = rw_resp.get("data") or {}
        collapsed = rw_data.get("collapsedMessages")
        if (
            rw_data.get("checkpoint") != "alpha"
            or not is_plain_int(collapsed)
            or collapsed < 2
            or rw_data.get("treePreserved") is not True
        ):
            return (
                1,
                int((time.monotonic() - t0) * 1000),
                f"RPC rewind did not collapse checkpoint span (data={rw_data})",
            )

        idle_rw, state_rw = wait_idle("rw")
        ok_stats_rw, stats_rw = fetch_stats("rw")
        if not idle_rw or not ok_stats_rw:
            return 1, int((time.monotonic() - t0) * 1000), "RPC post-rewind state/stats failed"

        count_after_rw = state_rw.get("messageCount")
        active_after_rw = stats_rw.get("totalMessages")
        if not all(
            is_plain_int(v)
            for v in (
                count_at_cp,
                count_after_p2,
                count_after_rw,
                active_at_cp,
                active_after_p2,
                active_after_rw,
            )
        ):
            return (
                1,
                int((time.monotonic() - t0) * 1000),
                f"RPC state/stats missing integer counts (tree={count_at_cp!r}->{count_after_p2!r}->{count_after_rw!r}, active={active_at_cp!r}->{active_after_p2!r}->{active_after_rw!r})",
            )
        # Rewind preserves raw JSONL tree entries (treePreserved=true, so
        # get_state.messageCount stays >= count_after_p2) while collapsing the
        # projected active model context reported by get_session_stats.totalMessages.
        if count_after_rw < count_after_p2:
            return (
                1,
                int((time.monotonic() - t0) * 1000),
                f"RPC rewind lost preserved tree entries (tree={count_at_cp}->{count_after_p2}->{count_after_rw})",
            )
        if not (active_after_p2 > active_at_cp and active_at_cp <= active_after_rw < active_after_p2):
            return (
                1,
                int((time.monotonic() - t0) * 1000),
                f"RPC rewind did not shrink projected active context (active={active_at_cp}->{active_after_p2}->{active_after_rw}, collapsed={collapsed})",
            )

        # Prompt 3 proves on the wire that the abandoned turn is gone from the
        # active provider request and replaced by the checkpoint rewind report.
        ok_p3, _ = send_and_wait(
            {"id": "p-3", "type": "prompt", "message": "Third turn after rewind"},
            wait_event="agent_end",
        )
        idle_p3, _ = wait_idle("p3")
        if not ok_p3 or not idle_p3:
            return 1, int((time.monotonic() - t0) * 1000), "RPC post-rewind prompt 3 failed"
        wire_msgs = stub.last_request_messages()
        wire_blob = json.dumps(wire_msgs)
        if "Second turn to rewind" in wire_blob or "REWIND REPORT" not in wire_blob:
            return (
                1,
                int((time.monotonic() - t0) * 1000),
                f"RPC post-rewind wire context did not collapse abandoned turn (messages={len(wire_msgs)})",
            )

        proc.stdin.close()
        rc = proc.wait(timeout=15)
        reader_thread.join(timeout=5)
        stderr_thread.join(timeout=5)
        elapsed = int((time.monotonic() - t0) * 1000)
        if rc != 0:
            return rc, elapsed, f"RPC shutdown exited {rc}: {''.join(stderr_chunks)[-400:]}"
        return (
            0,
            elapsed,
            f"RPC get_state + prompt + checkpoint + rewind (active {active_at_cp}->{active_after_p2}->{active_after_rw}, tree preserved={count_after_rw}, wire verified) + graceful shutdown succeeded",
        )
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=5)
        reader_thread.join(timeout=2)
        stderr_thread.join(timeout=2)


def self_test() -> int:
    ok1, _ = validate_version_output(
        "pi 0.9.0 (6e3d82b6eb1e38d96beb381d11a09fbeb311dd9e 2026-11-21)",
        expected_git_sha="6e3d82b6eb1e38d96beb381d11a09fbeb311dd9e",
    )
    assert ok1, "canonical 40-hex matching HEAD should pass"
    ok2, _ = validate_version_output(
        "pi 0.9.0 (6e3d82b 2026-11-21)",
        expected_git_sha="6e3d82b6eb1e38d96beb381d11a09fbeb311dd9e",
    )
    assert ok2, "canonical short-hex matching HEAD prefix should pass"
    bad_mismatch, _ = validate_version_output(
        "pi 0.9.0 (deadbeef 2026-11-21)",
        expected_git_sha="6e3d82b6eb1e38d96beb381d11a09fbeb311dd9e",
    )
    assert not bad_mismatch, "mismatched HEAD SHA must fail closed"
    bad1, _ = validate_version_output("pi 0.8.0 (unknown 2026-10-13T16:17:00Z)")
    assert not bad1, "unknown SHA must fail closed"
    bad2, _ = validate_version_output("")
    assert not bad2, "empty version must fail closed"
    rec = CheckRecord(
        check_name="self_test_probe",
        command="true",
        exit_code=0,
        expected_exit_code=0,
        duration_ms=1,
        git_sha="0" * 40,
        dsr_run_dir=None,
        verdict="pass",
        detail="ok",
    ).to_json()
    assert rec["schema"] == SCHEMA_ID
    print("SELF-TEST PASS")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Reality-Check Bridge (.10) E2E verification gate")
    parser.add_argument("--pi-bin", default=None, help="Path to pi binary to test")
    parser.add_argument("--dsr-run-dir", default=None, help="Authoritative DSR run dir to bind into records")
    parser.add_argument("--output-jsonl", required=False, default=None, help="Output JSONL bundle path")
    parser.add_argument("--expect-head-sha", action="store_true", help="Require --version commit SHA to match current git HEAD")
    parser.add_argument("--allow-unknown-version", action="store_true", help="Report version SHA check as skip/warn instead of failing (diagnostic only)")
    parser.add_argument("--live-default-model", action="store_true", help="Also execute live default-model probe via run_provider_live_e2e.py --check-defaults")
    parser.add_argument("--skip-governance", action="store_true", help="Skip repo governance scripts (installer/freshness/reachability/readiness)")
    parser.add_argument("--skip-br", action="store_true", help="Skip `br dep cycles` (use during an active DSR source freeze)")
    parser.add_argument("--self-test", action="store_true", help="Run internal planted-red self-tests and exit")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    pi_bin = args.pi_bin or auto_detect_pi_bin()
    if not pi_bin or not Path(pi_bin).is_file():
        print("ERROR: no executable pi binary found (pass --pi-bin PATH)", file=sys.stderr)
        return 2

    git_sha = resolve_git_sha()
    records: list[CheckRecord] = []

    def add(rec: CheckRecord) -> None:
        records.append(rec)
        print(f"[{rec.verdict.upper():4}] {rec.check_name} (rc={rec.exit_code}, {rec.duration_ms}ms): {rec.detail}")

    stub = LoopbackStubProvider()
    try:
        with tempfile.TemporaryDirectory(prefix="pi-rc-e2e-") as tmp:
            tmp_path = Path(tmp)
            _, stub_env = write_stub_agent_dir(tmp_path, stub.port)
            isolation = [
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-themes",
                "--thinking", "off",
            ]

            # 1. pi --version (canonical SHA, never unknown; optionally matches HEAD)
            cmd = [pi_bin, "--version"]
            rc, out, err, dur = run_cmd(cmd, cwd=tmp_path, env=stub_env)
            ver_ok, ver_detail = validate_version_output(
                out, expected_git_sha=git_sha if args.expect_head_sha else None
            )
            verdict = "pass" if (rc == 0 and ver_ok) else ("skip" if args.allow_unknown_version else "fail")
            add(CheckRecord("cli_version_canonical_sha", " ".join(cmd), rc, 0, dur, git_sha, args.dsr_run_dir, verdict, ver_detail if rc == 0 else err.strip()))

            # 2. pi doctor --format json (clean isolated user workspace)
            cmd = [pi_bin, "doctor", "--format", "json"]
            rc, out, err, dur = run_cmd(cmd, cwd=tmp_path, env=stub_env)
            doc_ok = False
            doc_detail = ""
            if rc == 0:
                try:
                    parsed = json.loads(out)
                    doc_ok = isinstance(parsed, dict)
                    doc_detail = f"valid JSON doctor output (keys={len(parsed)})"
                except json.JSONDecodeError as exc:
                    doc_detail = f"invalid JSON from doctor: {exc}"
            else:
                doc_detail = f"doctor exited {rc}: {(err or out).strip()[:240]}"
            add(CheckRecord("cli_doctor_json", " ".join(cmd), rc, 0, dur, git_sha, args.dsr_run_dir, "pass" if doc_ok else "fail", doc_detail))

            # 3. --wait-async parsing & execution
            stub.set_mode("ok")
            cmd = [
                pi_bin, "-p", "--mode", "json", "--no-session", "--wait-async",
                "--provider", "e2e-stub", "--model", "e2e-stub-model",
                *isolation, "Verify wait-async flag parsing.",
            ]
            rc, out, err, dur = run_cmd(cmd, cwd=tmp_path, env=stub_env)
            add(CheckRecord(
                "cli_wait_async_parses_cleanly",
                " ".join(cmd),
                rc,
                0,
                dur,
                git_sha,
                args.dsr_run_dir,
                "pass" if rc == 0 else "fail",
                "parsed and completed cleanly" if rc == 0 else (err or out).strip()[:240],
            ))

            # 4. RPC get_state + checkpoint + rewind + graceful shutdown
            rc, dur, detail = run_rpc_smoke(pi_bin, tmp_path, stub_env, stub)
            add(CheckRecord(
                "rpc_get_state_and_rewind",
                f"{pi_bin} --mode rpc --no-session",
                rc,
                0,
                dur,
                git_sha,
                args.dsr_run_dir,
                "pass" if rc == 0 else "fail",
                detail,
            ))

            # 5. Print --mode json exact injective exit codes 0..5
            # Exit 0
            stub.set_mode("ok")
            cmd0 = [
                pi_bin, "--print", "--mode", "json", "--no-session",
                "--provider", "e2e-stub", "--model", "e2e-stub-model",
                *isolation, "Say OK.",
            ]
            rc, out, err, dur = run_cmd(cmd0, cwd=tmp_path, env=stub_env)
            json_lines = [ln for ln in out.splitlines() if ln.strip()]
            json_valid = bool(json_lines)
            for ln in json_lines:
                try:
                    if not isinstance(json.loads(ln), dict):
                        json_valid = False
                        break
                except json.JSONDecodeError:
                    json_valid = False
                    break
            ok0 = rc == 0 and json_valid and "OK loopback turn 1" in out
            add(CheckRecord("json_exit_code_0_success", " ".join(cmd0), rc, 0, dur, git_sha, args.dsr_run_dir, "pass" if ok0 else "fail", f"rc={rc}, json_lines={len(json_lines)}, json_valid={json_valid}, has_expected_text={'OK loopback turn 1' in out}"))

            # Exit 1 (provider HTTP 401 non-retryable failure)
            stub.set_mode("http_401")
            cmd1 = [
                pi_bin, "--print", "--mode", "json", "--no-session",
                "--provider", "e2e-stub", "--model", "e2e-stub-model",
                *isolation, "Trigger provider 401.",
            ]
            rc, out, err, dur = run_cmd(cmd1, cwd=tmp_path, env=stub_env)
            combined1 = f"{out}\n{err}"
            ok1 = rc == 1 and ("401" in combined1 or "authentication" in combined1.lower())
            add(CheckRecord("json_exit_code_1_failure", " ".join(cmd1), rc, 1, dur, git_sha, args.dsr_run_dir, "pass" if ok1 else "fail", f"rc={rc}, detail={(err or out).strip()[:160]}"))

            # Exit 2 (usage error)
            cmd2 = [pi_bin, "--print", "--mode", "json", "--no-session", "--thinking", "invalid-level", "hi"]
            rc, out, err, dur = run_cmd(cmd2, cwd=tmp_path, env=stub_env)
            add(CheckRecord("json_exit_code_2_usage", " ".join(cmd2), rc, 2, dur, git_sha, args.dsr_run_dir, "pass" if rc == 2 else "fail", f"rc={rc}, detail={(err or out).strip()[:160]}"))

            # Exit 3 (headless approval surface unavailable)
            stub.set_mode("bash_tool_once")
            cmd3 = [
                pi_bin, "--print", "--mode", "json", "--no-session",
                "--provider", "e2e-stub", "--model", "e2e-stub-model",
                "--tools", "bash,ask",
                *isolation, "Run echo hi.",
            ]
            rc, out, err, dur = run_cmd(cmd3, cwd=tmp_path, env=stub_env)
            add(CheckRecord("json_exit_code_3_approval_unavailable", " ".join(cmd3), rc, 3, dur, git_sha, args.dsr_run_dir, "pass" if rc == 3 else "fail", f"rc={rc}, detail={(err or out).strip()[:160]}"))

            # Exit 4 (explicit missing --skill resource load failure)
            cmd4 = [
                pi_bin, "--print", "--mode", "json", "--no-session",
                "--no-extensions", "--no-prompt-templates", "--no-themes",
                "--skill", str(tmp_path / "missing-skill" / "SKILL.md"),
                "hello",
            ]
            rc, out, err, dur = run_cmd(cmd4, cwd=tmp_path, env=stub_env)
            add(CheckRecord("json_exit_code_4_resource_load_failed", " ".join(cmd4), rc, 4, dur, git_sha, args.dsr_run_dir, "pass" if rc == 4 else "fail", f"rc={rc}, detail={(err or out).strip()[:160]}"))

            # Exit 5 (maxCost cap exceeded before second prompt)
            stub.set_mode("expensive")
            cmd5 = [
                pi_bin, "--print", "--mode", "json", "--no-session",
                "--provider", "e2e-stub", "--model", "e2e-stub-model",
                "--max-cost", "0.000001",
                *isolation, "first prompt", "second prompt",
            ]
            rc, out, err, dur = run_cmd(cmd5, cwd=tmp_path, env=stub_env)
            add(CheckRecord("json_exit_code_5_cost_cap", " ".join(cmd5), rc, 5, dur, git_sha, args.dsr_run_dir, "pass" if rc == 5 else "fail", f"rc={rc}, detail={(err or out).strip()[:160]}"))

            # 6. Optional live default-model probe (routed into temp/output dir so tracked docs/evidence/ stays clean!)
            if args.live_default_model:
                live_out_dir = Path(args.output_jsonl).parent if args.output_jsonl else tmp_path
                live_out_dir.mkdir(parents=True, exist_ok=True)
                live_runpack = live_out_dir / "provider-live-validation-runpack.json"
                live_defaults = live_out_dir / "provider-default-probe.jsonl"
                cmd_live = [
                    sys.executable,
                    "scripts/perf/run_provider_live_e2e.py",
                    "--check-defaults",
                    "--binary", pi_bin,
                    "--out", str(live_runpack),
                    "--defaults-jsonl", str(live_defaults),
                ]
                rc, out, err, dur = run_cmd(cmd_live, cwd=REPO_ROOT, env=os.environ.copy(), timeout=180)
                live_passes = 0
                live_fails = 0
                if live_defaults.is_file():
                    for line in live_defaults.read_text(encoding="utf-8").splitlines():
                        if not line.strip():
                            continue
                        try:
                            row = json.loads(line)
                        except json.JSONDecodeError:
                            continue
                        if row.get("verdict") == "pass":
                            live_passes += 1
                        elif row.get("verdict") == "fail":
                            live_fails += 1
                if rc != 0 or live_fails > 0:
                    live_verdict = "fail"
                elif live_passes == 0:
                    live_verdict = "skip"
                else:
                    live_verdict = "pass"
                tail_live = (out or err).strip().splitlines()[-1][:200] if (out or err).strip() else f"rc={rc}"
                add(CheckRecord(
                    "live_default_model_probe",
                    " ".join(cmd_live),
                    rc,
                    0,
                    dur,
                    git_sha,
                    args.dsr_run_dir,
                    live_verdict,
                    f"live_passes={live_passes}, live_fails={live_fails}; {tail_live}",
                ))
    finally:
        stub.close()

    # 7. Supply chain, governance & tracker checks
    if not args.skip_governance:
        gov_checks: list[tuple[str, list[str]]] = [
            ("gov_installer_regression", ["bash", "tests/installer_regression.sh"]),
            ("gov_module_reachability", [sys.executable, "scripts/check_module_reachability.py"]),
            ("gov_fixture_read_patience", [sys.executable, "scripts/check_fixture_read_patience.py"]),
            ("gov_readme_evidence_freshness", [sys.executable, "scripts/check_readme_evidence_freshness.py"]),
            ("gov_dsr_contract_sync", [sys.executable, "scripts/check_dsr_contract_sync.py", "--check-host-if-present"]),
            ("gov_claim_readiness_selftest", [sys.executable, "scripts/report_swarm_claim_readiness.py", "--self-test"]),
        ]
        if not args.skip_br:
            gov_checks.append(("gov_beads_dep_cycles", ["br", "dep", "cycles"]))
        for name, cmd in gov_checks:
            rc, out, err, dur = run_cmd(cmd, cwd=REPO_ROOT, env=os.environ.copy(), timeout=300)
            tail = (out or err).strip().splitlines()[-1][:240] if (out or err).strip() else f"rc={rc}"
            add(CheckRecord(name, " ".join(cmd), rc, 0, dur, git_sha, args.dsr_run_dir, "pass" if rc == 0 else "fail", tail))

    if args.output_jsonl:
        out_path = Path(args.output_jsonl)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        with out_path.open("w", encoding="utf-8") as fh:
            for rec in records:
                fh.write(json.dumps(rec.to_json()) + "\n")
        print(f"Wrote {len(records)} verification records to {out_path}")

    failed = [r for r in records if r.verdict == "fail"]
    print(f"SUMMARY: total={len(records)} pass={sum(r.verdict == 'pass' for r in records)} skip={sum(r.verdict == 'skip' for r in records)} fail={len(failed)}")
    return 0 if not failed else 1


if __name__ == "__main__":
    raise SystemExit(main())
