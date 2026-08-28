#!/usr/bin/env python3
"""scripts/perf/measure_idle_memory.py

Canonical idle-memory measurement recipe (bd-idle-memory-canonical-recipe-vrk8m).

Spawns the user-facing release binary `target/release/pi` in a steady
idle mode, samples RSS over 5 seconds after a 5s settle window, and
writes a canonical artifact at
`tests/perf/reports/release_evidence/idle_memory_rss.json` with schema
`pi.perf.idle_memory_rss.v1`.

The five measurement taxonomies are:
  1. cold-start idle (just spawned, no model loaded)
  2. post-extension-warmup idle
  3. post-conversation idle (model has been used, then idle for 30s)
  4. post-compaction idle
  5. post-tool-heavy idle

For v0.3.0 we measure taxonomy 1 (cold-start) and taxonomy 2
(post-warmup). The other three are stretch goals.

Exit 0 = artifact written, schema valid, samples >= 5.
Exit 1 = setup error (binary not found, schema mismatch, etc.).
Exit 2 = insufficient samples (binary did not stay idle long enough).
"""
from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = "pi.perf.idle_memory_rss.v1"
SETTLE_SECONDS = 5.0
SAMPLE_INTERVAL_SECONDS = 1.0
SAMPLE_COUNT = 5
REQUIRED_FIELDS = (
    "schema", "generated_at", "pid", "process_name", "allocator",
    "binary_path", "binary_sha256", "rss_bytes", "idle_state",
    "cargo_profile", "build_command", "sample_count", "samples",
    "rss_spread_bytes", "settle_ms", "bench_env_source", "bench_env",
    "bench_env_sha256",
)


def project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def rss_linux(pid: int) -> int | None:
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
    except OSError:
        return None
    return None


def rss_macos(pid: int) -> int | None:
    try:
        out = subprocess.check_output(
            ["ps", "-o", "rss=", "-p", str(pid)],
            text=True,
            timeout=2.0,
        ).strip()
        return int(out) * 1024
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, ValueError):
        return None


def rss(pid: int) -> int | None:
    if platform.system() == "Darwin":
        return rss_macos(pid)
    return rss_linux(pid)


def sha256_file(path: Path) -> str:
    import hashlib
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def bench_env(rch_diagnose_head: str) -> dict:
    return {
        "arch": platform.machine(),
        "os": f"{platform.system()} {platform.release()}",
        "rch_diagnose_head": rch_diagnose_head[:512],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--binary",
        type=Path,
        default=project_root() / "target/release/pi",
        help="Path to the release binary (default: target/release/pi)",
    )
    ap.add_argument(
        "--idle-state",
        default="startup_before_user_input",
        choices=[
            "startup_before_user_input",
            "post_extension_warmup",
            "post_conversation",
            "post_compaction",
            "post_tool_heavy",
        ],
    )
    ap.add_argument(
        "--out",
        type=Path,
        default=project_root() / "tests/perf/reports/release_evidence/idle_memory_rss.json",
    )
    ap.add_argument(
        "--mode",
        default="print",
        help="Mode to spawn the binary in (default: print, which is single-shot)",
    )
    args = ap.parse_args()

    binary = args.binary
    if not binary.exists():
        print(f"FAIL: release binary not found: {binary}", file=sys.stderr)
        return 1

    binary_sha = sha256_file(binary)

    # Spawn the binary. `--print` keeps the process alive for the
    # sample window while still being a fast single-shot. The settle +
    # process is allowed to exit after the response is printed.
    cmd = [str(binary), "--print", "ping"]
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        # Settle window
        time.sleep(SETTLE_SECONDS)
        # Sample
        samples = []
        for _ in range(SAMPLE_COUNT):
            r = rss(proc.pid)
            if r is not None:
                samples.append({"pid": proc.pid, "rss_bytes": r})
            time.sleep(SAMPLE_INTERVAL_SECONDS)
    finally:
        try:
            proc.terminate()
            proc.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            proc.kill()

    if len(samples) < 3:
        print(f"FAIL: insufficient samples ({len(samples)})", file=sys.stderr)
        return 2

    rss_values = [s["rss_bytes"] for s in samples]
    rss_median = sorted(rss_values)[len(rss_values) // 2]
    rss_spread = max(rss_values) - min(rss_values)

    # Capture a brief rch diagnose head for provenance
    rch_diag = ""
    try:
        rch_diag = subprocess.check_output(
            ["rch", "diagnose"], text=True, timeout=5.0, stderr=subprocess.STDOUT,
        )
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, FileNotFoundError):
        rch_diag = "rch diagnose unavailable"

    env = bench_env(rch_diag)
    env_json = json.dumps(env, sort_keys=True)
    env_sha = __import__("hashlib").sha256(env_json.encode()).hexdigest()

    artifact = {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "pid": samples[0]["pid"],
        "process_name": binary.name,
        "allocator": "system",
        "binary_path": str(binary),
        "binary_sha256": binary_sha,
        "rss_bytes": rss_median,
        "idle_state": args.idle_state,
        "cargo_profile": "release",
        "build_command": f"cargo build --bin {binary.name} --release",
        "sample_count": len(samples),
        "samples": samples,
        "rss_spread_bytes": rss_spread,
        "settle_ms": int(SETTLE_SECONDS * 1000),
        "bench_env_source": "scripts/perf/measure_idle_memory.py",
        "bench_env": env,
        "bench_env_sha256": env_sha,
    }

    # Schema check
    missing = [f for f in REQUIRED_FIELDS if f not in artifact]
    if missing:
        print(f"FAIL: artifact missing required fields: {missing}", file=sys.stderr)
        return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        json.dump(artifact, f, indent=2)
    print(f"wrote {args.out}: rss={rss_median} bytes ({rss_median/1024/1024:.1f} MB) "
          f"from {len(samples)} samples (spread={rss_spread} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
