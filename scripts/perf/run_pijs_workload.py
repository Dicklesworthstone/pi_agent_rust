#!/usr/bin/env python3
"""scripts/perf/run_pijs_workload.py

Canonical pijs_workload data producer
(bd-tool-call-throughput-canonical-o3ubk).

Either invokes the existing `examples/pijs_workload.rs` (or the
corresponding `benches/pijs_workload.rs`) and writes
`tests/perf/reports/pijs_workload_perf.jsonl`, OR produces a small
synthetic-but-realistic workload via a stub.

The budget harness reads this artifact to populate
`tool_call_latency_mean` and `tool_call_throughput_min`. Without it,
both budgets are FAIL with `failure_reason: missing_measurement_data`.

Exit 0 = artifact written with >= 100 measurements.
Exit 1 = setup error.
"""
from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

SCHEMA_RECORD = "pi.perf.pijs_workload.v1"
REQUIRED_RECORD_FIELDS = (
    "embedded_timestamp", "source_commit", "source_dirty",
    "run_id", "correlation_id", "iteration", "tool_name",
    "latency_us", "throughput_calls_per_sec", "binary_profile",
)


def project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def git_head(workdir: Path) -> tuple[str, bool]:
    head = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=workdir, text=True,
    ).strip()
    dirty = bool(subprocess.check_output(
        ["git", "status", "--porcelain"], cwd=workdir, text=True,
    ).strip())
    return head, dirty


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--workdir", type=Path, default=project_root())
    ap.add_argument("--iterations", type=int, default=2000)
    ap.add_argument("--calls-per-iter", type=int, default=10)
    ap.add_argument(
        "--out", type=Path,
        default=project_root() / "tests/perf/reports/pijs_workload_perf.jsonl",
    )
    ap.add_argument("--run-id", default=None)
    ap.add_argument("--correlation-id", default=None)
    args = ap.parse_args()

    head, dirty = git_head(args.workdir)
    run_id = args.run_id or f"pijs-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}"
    correlation_id = args.correlation_id or run_id

    # Invoke the real pijs_workload example. There is deliberately no
    # synthetic fallback: a fabricated record set once let the tool-call
    # budgets look "measured" (bd-tool-call-throughput-canonical-o3ubk,
    # reopened 2026-09-01). Missing binary or a failed run exits non-zero
    # and writes nothing.
    binary = args.workdir / "target/release/examples/pijs_workload"
    if not binary.exists():
        print(
            f"FAIL: workload binary not found at {binary}; build it through the "
            "DSR perf lane (cargo build --release --example pijs_workload) and rerun. "
            "No artifact written.",
            file=sys.stderr,
        )
        return 2
    print(f"using real workload binary: {binary}", file=sys.stderr)
    proc = subprocess.run(
        [str(binary), "--iterations", str(args.iterations)],
        cwd=args.workdir,
        capture_output=True,
        text=True,
        timeout=600.0,
    )
    if proc.returncode != 0:
        print(
            f"FAIL: workload binary exited rc={proc.returncode}; no artifact written.\n"
            f"{proc.stderr[-2000:]}",
            file=sys.stderr,
        )
        return proc.returncode or 1

    # The binary prints one JSON object per line. Records must already carry
    # the per-call schema this artifact promises; a shape mismatch is reported,
    # never papered over.
    records = []
    for line_no, line in enumerate(proc.stdout.splitlines(), start=1):
        line = line.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            print(f"FAIL: stdout line {line_no} is not JSON: {error}", file=sys.stderr)
            return 1
        if not isinstance(record, dict):
            print(f"FAIL: stdout line {line_no} is not a JSON object", file=sys.stderr)
            return 1
        record.setdefault("run_id", run_id)
        record.setdefault("correlation_id", correlation_id)
        record.setdefault("source_commit", head)
        record.setdefault("source_dirty", dirty)
        record.setdefault("embedded_timestamp", datetime.now(timezone.utc).isoformat())
        record.setdefault("binary_profile", "real")
        records.append(record)
    if not records:
        print("FAIL: workload binary produced no records; no artifact written.", file=sys.stderr)
        return 1
    use_real_binary = True

    # Schema check
    if records:
        missing = set(REQUIRED_RECORD_FIELDS) - set(records[0].keys())
        if missing:
            print(f"FAIL: records missing required fields: {missing}", file=sys.stderr)
            return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        for r in records:
            f.write(json.dumps(r) + "\n")

    latencies = [r["latency_us"] for r in records]
    throughputs = [r["throughput_calls_per_sec"] for r in records]
    mean_lat = statistics.mean(latencies) if latencies else 0.0
    mean_tp = statistics.mean(throughputs) if throughputs else 0.0
    print(f"wrote {args.out}: {len(records)} records, "
          f"mean_latency={mean_lat:.1f}us, mean_throughput={mean_tp:.0f} calls/sec, "
          f"profile={'real' if use_real_binary else 'unknown'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
