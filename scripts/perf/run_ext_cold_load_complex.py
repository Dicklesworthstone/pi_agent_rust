#!/usr/bin/env python3
"""scripts/perf/run_ext_cold_load_complex.py

Summarizes real Criterion cold-load estimates (and sample distributions when
present) for the `ext_cold_load_complex_p95` budget.

There is deliberately NO synthetic or PRNG fallback: if real Criterion data for
the requested fixture does not exist, this script exits 2 and writes nothing.

Exit 0 = real Criterion summary written.
Exit 1 = malformed Criterion input.
Exit 2 = Criterion estimates not found.
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

SCHEMA = "pi.perf.criterion_estimate.v1"
DEFAULT_BUDGET_US = 50_000.0


def project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def find_estimates_path(
    workdir: Path,
    fixture: str,
    explicit_estimates: Path | None,
    criterion_dir: Path | None,
) -> Path | None:
    if explicit_estimates is not None:
        return explicit_estimates if explicit_estimates.is_file() else None

    bases: list[Path] = []
    if criterion_dir is not None:
        bases.append(criterion_dir)
    env_target = os.environ.get("CARGO_TARGET_DIR", "").strip()
    if env_target:
        target_path = Path(env_target)
        if not target_path.is_absolute():
            target_path = workdir / target_path
        bases.append(target_path / "criterion")
    bases.append(workdir / "target/criterion")

    rel = Path(f"ext_load_init/load_init_cold/{fixture}/new/estimates.json")
    for base in bases:
        direct = base / rel
        if direct.is_file():
            return direct
        if base.is_dir():
            isolated = sorted(
                (p for p in base.glob(f"pi-perf-runs/*/criterion_extensions/{rel}") if p.is_file()),
                key=lambda p: p.stat().st_mtime,
                reverse=True,
            )
            if isolated:
                return isolated[0]
    return None


def is_num_strict(val: Any) -> bool:
    return isinstance(val, (int, float)) and not isinstance(val, bool) and math.isfinite(val)


def read_sample_us(sample_path: Path) -> tuple[list[float] | None, str | None]:
    if not sample_path.is_file():
        return None, None
    try:
        data: Any = json.loads(sample_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        return None, f"invalid Criterion sample file {sample_path}: {exc}"
    if not isinstance(data, dict):
        return None, f"Criterion sample file {sample_path} must be a JSON object"
    iters = data.get("iters")
    times = data.get("times")
    if not isinstance(iters, list) or not isinstance(times, list) or not iters or len(iters) != len(times):
        return None, f"Criterion sample file {sample_path} has invalid iters/times arrays"
    per_iter_us: list[float] = []
    for it_val, ns_val in zip(iters, times):
        if not is_num_strict(it_val) or not is_num_strict(ns_val) or it_val <= 0 or ns_val <= 0:
            return None, f"Criterion sample file {sample_path} contains non-positive or non-numeric sample"
        per_iter_us.append((float(ns_val) / float(it_val)) / 1000.0)
    per_iter_us.sort()
    return per_iter_us, None


def percentile(sorted_values: list[float], p: float) -> float:
    idx = min(len(sorted_values) - 1, max(0, int(math.ceil(p * len(sorted_values))) - 1))
    return sorted_values[idx]


def run_self_test() -> int:
    old_target = os.environ.pop("CARGO_TARGET_DIR", None)
    try:
        with tempfile.TemporaryDirectory(prefix="ext_cold_load_selftest_") as tmp:
            root = Path(tmp)
            out_path = root / "ext_cold_load_complex_estimate.json"

            # 1. Missing Criterion data fails closed (exit 2) and writes nothing.
            rc = run_cli(["--workdir", str(root), "--out", str(out_path)])
            assert rc == 2, f"expected rc=2 when Criterion data missing, got {rc}"
            assert not out_path.exists(), "must not write synthetic fallback artifact"

            # 2. Real Criterion estimates.json + sample.json computes exact percentiles.
            crit_new = root / "target/criterion/ext_load_init/load_init_cold/pirate/new"
            crit_new.mkdir(parents=True, exist_ok=True)
            estimates_path = crit_new / "estimates.json"
            estimates_path.write_text(
                json.dumps(
                    {
                        "mean": {"point_estimate": 25_000_000.0},
                        "median": {"point_estimate": 24_000_000.0},
                        "std_dev": {"point_estimate": 1_500_000.0},
                    }
                ),
                encoding="utf-8",
            )
            sample_path = crit_new / "sample.json"
            # 20 samples: 1..20 ms (in ns)
            sample_path.write_text(
                json.dumps(
                    {
                        "iters": [1.0] * 20,
                        "times": [float(i * 1_000_000) for i in range(1, 21)],
                    }
                ),
                encoding="utf-8",
            )
            rc = run_cli(["--workdir", str(root), "--out", str(out_path)])
            assert rc == 0, f"expected rc=0 for valid Criterion estimates, got {rc}"
            artifact = json.loads(out_path.read_text(encoding="utf-8"))
            assert artifact["sample_count"] == 20
            assert artifact["p95_us"] == 19_000.0
            assert artifact["p99_us"] == 20_000.0
            assert artifact["status"] == "PASS"
            assert artifact["percentile_source"] == "criterion_sample_json"
            assert artifact["estimates_sha256"] == sha256_file(estimates_path)

            # 3. Negative controls: invalid budget, path-traversal fixture, malformed sample.json, and null median fail closed.
            neg_out = root / "neg_cold_load.json"
            assert run_cli(["--workdir", str(root), "--out", str(neg_out), "--budget-threshold-us", "0"]) == 1
            assert run_cli(["--workdir", str(root), "--out", str(neg_out), "--budget-threshold-us", "nan"]) == 1
            assert run_cli(["--workdir", str(root), "--out", str(neg_out), "--fixture", "../escape"]) == 1
            sample_path.write_text("{not valid json", encoding="utf-8")
            assert run_cli(["--workdir", str(root), "--out", str(neg_out)]) == 1
            sample_path.unlink()
            estimates_path.write_text(json.dumps({"mean": {"point_estimate": 1000.0}, "median": None}), encoding="utf-8")
            assert run_cli(["--workdir", str(root), "--out", str(neg_out)]) == 1
            assert not neg_out.exists()
    finally:
        if old_target is not None:
            os.environ["CARGO_TARGET_DIR"] = old_target

    print("run_ext_cold_load_complex.py --self-test: PASS")
    return 0


def run_cli(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--workdir", type=Path, default=project_root())
    ap.add_argument("--criterion-dir", type=Path, default=None)
    ap.add_argument("--estimates", type=Path, default=None, help="Explicit Criterion estimates.json path")
    ap.add_argument(
        "--out",
        type=Path,
        default=project_root() / "tests/perf/reports/ext_cold_load_complex_estimate.json",
    )
    ap.add_argument("--fixture", default="pirate")
    ap.add_argument("--budget-threshold-us", type=float, default=DEFAULT_BUDGET_US)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)

    if args.self_test:
        return run_self_test()

    if not args.fixture or "/" in args.fixture or "\\" in args.fixture or args.fixture in (".", ".."):
        print(f"FAIL: --fixture must be a safe fixture name, got {args.fixture!r}", file=sys.stderr)
        return 1
    if not math.isfinite(args.budget_threshold_us) or args.budget_threshold_us <= 0.0:
        print(
            f"FAIL: --budget-threshold-us must be finite and > 0, got {args.budget_threshold_us}",
            file=sys.stderr,
        )
        return 1

    estimates_path = find_estimates_path(args.workdir, args.fixture, args.estimates, args.criterion_dir)
    if estimates_path is None:
        print(
            f"FAIL: no Criterion estimates.json found for fixture {args.fixture!r}; "
            "run Criterion `ext_load_init` through the DSR/perf lane first. "
            "Refusing to fabricate synthetic estimates; no artifact written.",
            file=sys.stderr,
        )
        return 2

    try:
        est = json.loads(estimates_path.read_text(encoding="utf-8"))
        if not isinstance(est, dict):
            raise ValueError("estimates.json root must be a JSON object")
        mean_ns = est["mean"]["point_estimate"]
        median_ns = est.get("median", {}).get("point_estimate", mean_ns)
        stddev_ns = est.get("std_dev", {}).get("point_estimate", 0.0)
        if not is_num_strict(mean_ns) or mean_ns <= 0:
            raise ValueError(f"invalid mean.point_estimate={mean_ns!r}")
        if not is_num_strict(median_ns) or median_ns <= 0:
            raise ValueError(f"invalid median.point_estimate={median_ns!r}")
        if not is_num_strict(stddev_ns) or stddev_ns < 0:
            raise ValueError(f"invalid std_dev.point_estimate={stddev_ns!r}")
    except (OSError, json.JSONDecodeError, KeyError, TypeError, ValueError, AttributeError) as exc:
        print(f"FAIL: invalid Criterion estimates {estimates_path}: {exc}", file=sys.stderr)
        return 1

    sample_path = estimates_path.with_name("sample.json")
    samples_us, sample_err = read_sample_us(sample_path)
    if sample_err is not None:
        print(f"FAIL: {sample_err} (no artifact written)", file=sys.stderr)
        return 1
    if samples_us is not None:
        p50 = percentile(samples_us, 0.50)
        p95 = percentile(samples_us, 0.95)
        p99 = percentile(samples_us, 0.99)
        mean_us = statistics.mean(samples_us)
        stddev_us = statistics.stdev(samples_us) if len(samples_us) > 1 else 0.0
        sample_count = len(samples_us)
        percentile_source = "criterion_sample_json"
    else:
        mean_us = float(mean_ns) / 1000.0
        p50 = float(median_ns) / 1000.0
        p95 = mean_us
        p99 = mean_us
        stddev_us = float(stddev_ns) / 1000.0
        sample_count = 1
        percentile_source = "criterion_estimates_mean_only"

    artifact = {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
        "fixture": args.fixture,
        "budget_name": "ext_cold_load_complex_p95",
        "budget_threshold_us": args.budget_threshold_us,
        "evidence_class": "measured",
        "percentile_source": percentile_source,
        "sample_count": sample_count,
        "p50_us": round(p50, 3),
        "p95_us": round(p95, 3),
        "p99_us": round(p99, 3),
        "mean_us": round(mean_us, 3),
        "stddev_us": round(stddev_us, 3),
        "status": "PASS" if p95 <= args.budget_threshold_us else "FAIL",
        "source": f"criterion:{estimates_path.resolve()}",
        "estimates_sha256": sha256_file(estimates_path),
    }

    args.out.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = args.out.with_name(f".{args.out.name}.tmp.{os.getpid()}")
    try:
        tmp_path.write_text(json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        tmp_path.replace(args.out)
    except Exception:
        tmp_path.unlink(missing_ok=True)
        raise

    print(
        f"wrote {args.out}: fixture={args.fixture} ({percentile_source}, n={sample_count}) "
        f"p50={p50:.1f}us p95={p95:.1f}us (threshold {args.budget_threshold_us:.0f}us) "
        f"status={artifact['status']}"
    )
    return 0


def main() -> int:
    return run_cli()


if __name__ == "__main__":
    sys.exit(main())
