#!/usr/bin/env python3
"""scripts/perf/run_event_dispatch_scenario.py

Summarizes real `scenario_runner.jsonl` measurements for the `event_dispatch_p99`
budget (`tests/perf_budgets.rs::read_scenario_runner_per_call`).

There is deliberately NO synthetic or PRNG fallback: if real
`scenario_runner.jsonl` records for `scenario == "event_dispatch"` do not exist,
this script exits 2 and writes nothing.

Exit 0 = real summary written (`PASS` or `FAIL` reported honestly).
Exit 1 = malformed input or contract failure.
Exit 2 = real `scenario_runner.jsonl` measurement missing.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import statistics
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SCHEMA = "pi.perf.scenario_runner.v1"
FORBIDDEN_SYNTHETIC_MARKERS = ("synthetic", "stub", "fake", "mock")


def project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def candidate_inputs(
    workdir: Path,
    explicit_input: Path | None,
    evidence_dir: Path | None,
) -> list[Path]:
    if explicit_input is not None:
        return [explicit_input]
    candidates: list[Path] = []
    if evidence_dir is not None:
        for rel in (
            "scenario_runner.jsonl",
            "results/scenario_runner.jsonl",
            "perf/scenario_runner.jsonl",
            "perf/results/scenario_runner.jsonl",
        ):
            candidates.append(evidence_dir / rel)
    env_target = os.environ.get("CARGO_TARGET_DIR", "").strip()
    if env_target:
        target_path = Path(env_target)
        if not target_path.is_absolute():
            target_path = workdir / target_path
        candidates.append(target_path / "perf/scenario_runner.jsonl")
        candidates.append(target_path / "perf/results/scenario_runner.jsonl")
    candidates.extend(
        [
            workdir / "target/perf/scenario_runner.jsonl",
            workdir / "target/perf/results/scenario_runner.jsonl",
            workdir / "tests/perf/reports/scenario_runner.jsonl",
        ]
    )
    deduped: list[Path] = []
    for path in candidates:
        if path not in deduped:
            deduped.append(path)
    return deduped


def percentile(sorted_values: list[float], p: float) -> float:
    if not sorted_values:
        raise ValueError("empty sample list")
    idx = min(len(sorted_values) - 1, max(0, int(math.ceil(p * len(sorted_values))) - 1))
    return sorted_values[idx]


def load_real_event_dispatch_samples(path: Path) -> tuple[list[float], dict[str, int], str | None]:
    per_call_values: list[float] = []
    extension_counts: dict[str, int] = {}
    for line_no, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        line = raw_line.strip()
        if not line:
            continue
        try:
            record: Any = json.loads(line)
        except json.JSONDecodeError as exc:
            return [], {}, f"{path}:{line_no} invalid JSON: {exc}"
        if not isinstance(record, dict):
            return [], {}, f"{path}:{line_no} record is not a JSON object"
        if record.get("scenario") != "event_dispatch":
            continue
        for marker_field in ("evidence_class", "binary_profile", "source"):
            val = str(record.get(marker_field, "")).lower()
            if any(marker in val for marker in FORBIDDEN_SYNTHETIC_MARKERS):
                return (
                    [],
                    {},
                    f"{path}:{line_no} refuses synthetic/stub record ({marker_field}={record.get(marker_field)!r})",
                )
        per_call = record.get("per_call_us")
        if (
            isinstance(per_call, bool)
            or not isinstance(per_call, (int, float))
            or not math.isfinite(per_call)
            or per_call <= 0.0
        ):
            return (
                [],
                {},
                f"{path}:{line_no} event_dispatch record has invalid per_call_us={per_call!r}",
            )
        per_call_values.append(float(per_call))
        ext_name = str(record.get("extension") or record.get("extension_id") or "unknown")
        extension_counts[ext_name] = extension_counts.get(ext_name, 0) + 1
    return per_call_values, extension_counts, None


def run_self_test() -> int:
    old_target = os.environ.pop("CARGO_TARGET_DIR", None)
    try:
        with tempfile.TemporaryDirectory(prefix="event_dispatch_selftest_") as tmp:
            root = Path(tmp)
            out_path = root / "event_dispatch_scenario.json"

            # 1. Missing scenario_runner.jsonl fails closed (exit 2) and writes nothing.
            rc = run_cli(["--workdir", str(root), "--out", str(out_path)])
            assert rc == 2, f"expected rc=2 on missing scenario_runner.jsonl, got {rc}"
            assert not out_path.exists(), "must not fabricate artifact when source is missing"

            # 2. Synthetic record is rejected fail-closed (exit 1).
            synthetic_in = root / "synthetic.jsonl"
            synthetic_in.write_text(
                json.dumps(
                    {
                        "scenario": "event_dispatch",
                        "extension": "hello",
                        "per_call_us": 50.0,
                        "evidence_class": "synthetic_stub",
                    }
                )
                + "\n",
                encoding="utf-8",
            )
            rc = run_cli(["--input", str(synthetic_in), "--out", str(out_path)])
            assert rc == 1, f"expected rc=1 for synthetic record, got {rc}"
            assert not out_path.exists()

            # 3. Real scenario_runner.jsonl records summarize accurately.
            real_in = root / "scenario_runner.jsonl"
            rows = [
                {"scenario": "event_dispatch", "extension": "hello", "per_call_us": 120.5, "evidence_class": "measured"},
                {"scenario": "event_dispatch", "extension": "pirate", "per_call_us": 340.0, "evidence_class": "measured"},
                {"scenario": "tool_call", "extension": "hello", "per_call_us": 9999.0, "evidence_class": "measured"},
            ]
            real_in.write_text("\n".join(json.dumps(r) for r in rows) + "\n", encoding="utf-8")
            rc = run_cli(["--input", str(real_in), "--out", str(out_path), "--budget-p99-us", "5000.0"])
            assert rc == 0, f"expected rc=0 for real scenario_runner.jsonl, got {rc}"
            artifact = json.loads(out_path.read_text(encoding="utf-8"))
            assert artifact["sample_count"] == 2
            assert artifact["max_per_call_us"] == 340.0
            assert artifact["status"] == "PASS"
            assert artifact["source_sha256"] == sha256_file(real_in)

            # 4. Negative controls: boolean per_call_us and invalid budgets fail closed.
            bool_in = root / "bool_sample.jsonl"
            bool_in.write_text(
                json.dumps({"scenario": "event_dispatch", "extension": "hello", "per_call_us": True}) + "\n",
                encoding="utf-8",
            )
            neg_out = root / "neg_event_dispatch.json"
            assert run_cli(["--input", str(bool_in), "--out", str(neg_out)]) == 1
            assert run_cli(["--input", str(real_in), "--out", str(neg_out), "--budget-p99-us", "0"]) == 1
            assert run_cli(["--input", str(real_in), "--out", str(neg_out), "--budget-p99-us", "nan"]) == 1
            assert not neg_out.exists()
    finally:
        if old_target is not None:
            os.environ["CARGO_TARGET_DIR"] = old_target

    print("run_event_dispatch_scenario.py --self-test: PASS")
    return 0


def run_cli(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--workdir", type=Path, default=project_root())
    ap.add_argument("--input", type=Path, default=None, help="Path to real scenario_runner.jsonl")
    ap.add_argument("--evidence-dir", type=Path, default=None)
    ap.add_argument(
        "--out",
        type=Path,
        default=project_root() / "tests/perf/reports/event_dispatch_scenario.json",
    )
    ap.add_argument(
        "--budget-p99-us",
        type=float,
        default=5000.0,
        help="Budget threshold in microseconds (default 5000us)",
    )
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)

    if args.self_test:
        return run_self_test()

    if not math.isfinite(args.budget_p99_us) or args.budget_p99_us <= 0.0:
        print(f"FAIL: --budget-p99-us must be finite and > 0, got {args.budget_p99_us}", file=sys.stderr)
        return 1

    candidates = candidate_inputs(args.workdir, args.input, args.evidence_dir)
    source_path = next((p for p in candidates if p.is_file()), None)
    if source_path is None:
        searched = ", ".join(str(p) for p in candidates)
        print(
            f"FAIL: no real scenario_runner.jsonl found (searched: {searched}); "
            "produce it via the DSR/perf lane (`bench_extension_scenarios`) before summarizing. "
            "Refusing to fabricate synthetic numbers; no artifact written.",
            file=sys.stderr,
        )
        return 2

    samples, ext_counts, err = load_real_event_dispatch_samples(source_path)
    if err is not None:
        print(f"FAIL: {err} (no artifact written)", file=sys.stderr)
        return 1
    if not samples:
        print(
            f"FAIL: {source_path} contains zero `scenario == \"event_dispatch\"` records "
            "(no artifact written).",
            file=sys.stderr,
        )
        return 2

    ordered = sorted(samples)
    p50 = percentile(ordered, 0.50)
    p95 = percentile(ordered, 0.95)
    p99 = percentile(ordered, 0.99)
    max_us = ordered[-1]
    mean_us = statistics.mean(ordered)

    artifact = {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
        "scenario": "event_dispatch",
        "evidence_class": "measured",
        "source_path": str(source_path.resolve()),
        "source_sha256": sha256_file(source_path),
        "sample_count": len(ordered),
        "extension_counts": ext_counts,
        "latency_p50_us": round(p50, 3),
        "latency_p95_us": round(p95, 3),
        "latency_p99_us": round(p99, 3),
        "latency_mean_us": round(mean_us, 3),
        "max_per_call_us": round(max_us, 3),
        "budget_p99_us": args.budget_p99_us,
        "status": "PASS" if max_us <= args.budget_p99_us else "FAIL",
    }

    args.out.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = args.out.with_suffix(args.out.suffix + ".tmp")
    tmp_path.write_text(json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp_path.replace(args.out)

    print(
        f"wrote {args.out}: samples={len(ordered)} max={max_us:.3f}us p99={p99:.3f}us "
        f"(threshold {args.budget_p99_us}us) status={artifact['status']}"
    )
    return 0


def main() -> int:
    return run_cli()


if __name__ == "__main__":
    sys.exit(main())
