#!/usr/bin/env python3
"""scripts/perf/run_pijs_workload.py

Canonical PiJS workload runner (bd-tool-call-throughput-canonical-o3ubk).

Invokes the real `examples/pijs_workload.rs` perf-profile binary for BOTH
regression-gate lanes (2000 iterations x 1 tool call for
`tool_call_latency_mean`, and 2000 iterations x 10 tool calls for
`tool_call_throughput_min`), validates the resulting `pi.perf.workload.v1`
JSONL pair against the exact `tests/perf_budgets.rs` contract, and atomically
writes `tests/perf/reports/pijs_workload_perf.jsonl`.

There is deliberately NO synthetic fallback: a missing binary, wrong profile,
or schema/lineage violation exits non-zero and writes nothing.

Exit 0 = canonical two-lane `pi.perf.workload.v1` pair written and verified.
Exit 1 = runtime failure or contract validation failure.
Exit 2 = perf-profile `pijs_workload` binary not found.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import stat
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SCHEMA_RECORD = "pi.perf.workload.v1"
CANONICAL_ITERATIONS = 2000
CANONICAL_TOOL_CALL_LANES = (1, 10)
CANONICAL_PIJS_PERF_FEATURES = [
    "clipboard",
    "image",
    "image-resize",
    "sqlite-sessions",
    "wasm-host",
]
REQUIRED_RECORD_FIELDS = (
    "schema",
    "timestamp",
    "run_id",
    "correlation_id",
    "source_commit",
    "source_dirty",
    "tool",
    "scenario",
    "iterations",
    "tool_calls_per_iteration",
    "total_calls",
    "elapsed_ms",
    "elapsed_us",
    "elapsed_us_f64",
    "per_call_us",
    "per_call_us_f64",
    "calls_per_sec",
    "build_profile",
    "build_profile_verified",
    "build_fingerprint_contract",
    "build_fingerprint_verified",
    "executable_build_profile",
    "executable_profile_verified",
    "compiled_profile_family",
    "compiled_opt_level",
    "compiled_debug",
    "compiled_features",
    "debug_assertions",
    "config_hash",
    "allocator_requested",
    "allocator_request_source",
    "allocator_effective",
    "runtime_engine",
    "evidence_class",
    "confidence",
    "eligible_for_regression_gate",
    "binary_path",
    "binary_sha256",
)
SHARED_PAIR_FIELDS = (
    "run_id",
    "correlation_id",
    "source_commit",
    "source_dirty",
    "build_profile",
    "runtime_engine",
    "binary_sha256",
    "compiled_features",
)


def is_int_strict(val: Any) -> bool:
    return isinstance(val, int) and not isinstance(val, bool)


def is_num_strict(val: Any) -> bool:
    return isinstance(val, (int, float)) and not isinstance(val, bool) and math.isfinite(val)


def project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def candidate_binaries(workdir: Path, explicit_binary: Path | None) -> list[Path]:
    if explicit_binary is not None:
        return [explicit_binary]
    candidates: list[Path] = []
    env_target = os.environ.get("CARGO_TARGET_DIR", "").strip()
    if env_target:
        target_path = Path(env_target)
        if not target_path.is_absolute():
            target_path = workdir / target_path
        candidates.append(target_path / "perf/examples/pijs_workload")
    candidates.append(workdir / "target/perf/examples/pijs_workload")
    deduped: list[Path] = []
    for path in candidates:
        if path not in deduped:
            deduped.append(path)
    return deduped


def resolve_binary(workdir: Path, explicit_binary: Path | None) -> Path | None:
    for path in candidate_binaries(workdir, explicit_binary):
        if path.is_file():
            return path
    return None


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_lane_record(
    record: dict[str, Any],
    *,
    expected_iterations: int,
    expected_tool_calls: int,
    expected_run_id: str,
    expected_correlation_id: str,
    expected_binary_sha256: str,
    expected_allocator: str,
    require_gate_eligible: bool,
) -> list[str]:
    errors: list[str] = []
    missing = [f for f in REQUIRED_RECORD_FIELDS if f not in record]
    if missing:
        errors.append(f"missing required fields: {missing}")
        return errors

    if record["schema"] != SCHEMA_RECORD:
        errors.append(f"schema must equal {SCHEMA_RECORD!r}, got {record['schema']!r}")
    if record["tool"] != "pijs_workload":
        errors.append(f"tool must equal 'pijs_workload', got {record['tool']!r}")
    if record["scenario"] != "tool_call_roundtrip":
        errors.append(
            f"scenario must equal 'tool_call_roundtrip', got {record['scenario']!r}"
        )
    if record["runtime_engine"] != "quickjs":
        errors.append(
            f"runtime_engine must equal 'quickjs', got {record['runtime_engine']!r}"
        )
    if record["evidence_class"] != "measured":
        errors.append(
            f"evidence_class must equal 'measured', got {record['evidence_class']!r}"
        )
    if record["run_id"] != expected_run_id:
        errors.append(
            f"run_id mismatch: expected {expected_run_id!r}, got {record['run_id']!r}"
        )
    if record["correlation_id"] != expected_correlation_id:
        errors.append(
            f"correlation_id mismatch: expected {expected_correlation_id!r}, got {record['correlation_id']!r}"
        )
    if record["binary_sha256"] != expected_binary_sha256:
        errors.append(
            f"binary_sha256 mismatch: expected {expected_binary_sha256!r}, got {record['binary_sha256']!r}"
        )
    if record["allocator_requested"] != expected_allocator:
        errors.append(
            f"allocator_requested mismatch: expected {expected_allocator!r}, got {record['allocator_requested']!r}"
        )
    if record["allocator_request_source"] != "env":
        errors.append(
            f"allocator_request_source must equal 'env', got {record['allocator_request_source']!r}"
        )

    iterations = record["iterations"]
    tool_calls = record["tool_calls_per_iteration"]
    total_calls = record["total_calls"]
    if not is_int_strict(iterations) or iterations != expected_iterations:
        errors.append(f"iterations expected {expected_iterations}, got {iterations!r}")
    if not is_int_strict(tool_calls) or tool_calls != expected_tool_calls:
        errors.append(
            f"tool_calls_per_iteration expected {expected_tool_calls}, got {tool_calls!r}"
        )
    if is_int_strict(iterations) and is_int_strict(tool_calls):
        if not is_int_strict(total_calls) or total_calls != iterations * tool_calls:
            errors.append(
                f"total_calls ({total_calls!r}) != iterations * tool_calls_per_iteration ({iterations * tool_calls})"
            )

    elapsed_us = record["elapsed_us"]
    elapsed_us_f64 = record["elapsed_us_f64"]
    per_call_us = record["per_call_us"]
    per_call_us_f64 = record["per_call_us_f64"]
    calls_per_sec = record["calls_per_sec"]

    if not is_int_strict(elapsed_us) or elapsed_us <= 0:
        errors.append(f"elapsed_us must be a positive int, got {elapsed_us!r}")
    elif not is_num_strict(elapsed_us_f64):
        errors.append(f"elapsed_us_f64 must be finite float, got {elapsed_us_f64!r}")
    elif not (elapsed_us <= elapsed_us_f64 < elapsed_us + 1):
        errors.append(
            f"elapsed_us ({elapsed_us}) must equal floor(elapsed_us_f64={elapsed_us_f64})"
        )
    elif is_int_strict(total_calls) and total_calls > 0:
        expected_int_lat = elapsed_us // total_calls
        if not is_int_strict(per_call_us) or per_call_us != expected_int_lat:
            errors.append(
                f"per_call_us ({per_call_us!r}) != elapsed_us // total_calls ({expected_int_lat})"
            )
        expected_int_tp = (total_calls * 1_000_000) // elapsed_us
        if not is_int_strict(calls_per_sec) or calls_per_sec != expected_int_tp:
            errors.append(
                f"calls_per_sec ({calls_per_sec!r}) != total_calls * 1e6 // elapsed_us ({expected_int_tp})"
            )
        derived_lat = elapsed_us_f64 / float(total_calls)
        tol = max(abs(derived_lat), 1.0) * sys.float_info.epsilon * 16.0
        if not is_num_strict(per_call_us_f64) or abs(per_call_us_f64 - derived_lat) > tol:
            errors.append(
                f"per_call_us_f64 ({per_call_us_f64!r}) inconsistent with elapsed_us_f64 / total_calls ({derived_lat})"
            )

    if require_gate_eligible:
        source_commit = str(record["source_commit"]).strip()
        if (
            len(source_commit) != 40
            or any(ch not in "0123456789abcdef" for ch in source_commit)
            or all(ch == "0" for ch in source_commit)
        ):
            errors.append(
                f"source_commit must be a 40-char lowercase non-zero git SHA, got {source_commit!r}"
            )
        if record["source_dirty"] is not False:
            errors.append("source_dirty must be false for regression-gate evidence")
        if record["build_profile"] != "perf":
            errors.append(
                f"build_profile must equal 'perf' for regression-gate eligibility, got {record['build_profile']!r}"
            )
        if record["build_fingerprint_contract"] != "cargo_build_fingerprint.v1":
            errors.append(
                "build_fingerprint_contract must equal 'cargo_build_fingerprint.v1', "
                f"got {record['build_fingerprint_contract']!r}"
            )
        if record["executable_build_profile"] != "perf":
            errors.append(
                f"executable_build_profile must equal 'perf', got {record['executable_build_profile']!r}"
            )
        config_hash = str(record["config_hash"]).strip()
        if (
            len(config_hash) != 64
            or any(ch not in "0123456789abcdef" for ch in config_hash)
            or all(ch == "0" for ch in config_hash)
        ):
            errors.append(
                f"config_hash must be a 64-char lowercase non-zero hex SHA-256, got {config_hash!r}"
            )
        if record["confidence"] != "high":
            errors.append(
                f"confidence must equal 'high' for regression-gate evidence, got {record['confidence']!r}"
            )
        for bool_field in (
            "eligible_for_regression_gate",
            "build_profile_verified",
            "build_fingerprint_verified",
            "executable_profile_verified",
        ):
            if record[bool_field] is not True:
                errors.append(f"{bool_field} must equal true for regression-gate evidence")
        if record["debug_assertions"] is not False:
            errors.append("debug_assertions must equal false for regression-gate evidence")
        if (
            record["compiled_profile_family"] != "release"
            or record["compiled_opt_level"] != "3"
            or record["compiled_debug"] != "true"
        ):
            errors.append(
                "compiled Cargo fingerprint must equal (family='release', opt_level='3', debug='true')"
            )
        if not isinstance(record["compiled_features"], list) or sorted(record["compiled_features"]) != CANONICAL_PIJS_PERF_FEATURES:
            errors.append(
                f"compiled_features must equal {CANONICAL_PIJS_PERF_FEATURES!r}, got {record['compiled_features']!r}"
            )
        if (
            expected_allocator != "system"
            or record["allocator_requested"] != "system"
            or record["allocator_request_source"] != "env"
            or record["allocator_effective"] != "system"
            or record.get("allocator_fallback_reason") is not None
        ):
            errors.append("allocator must match canonical system/env lane with no fallback")

    return errors


def validate_pair(
    records: list[dict[str, Any]],
    *,
    expected_iterations: int,
    expected_run_id: str,
    expected_correlation_id: str,
    expected_binary_sha256: str,
    expected_allocator: str,
    require_gate_eligible: bool,
) -> list[str]:
    if len(records) != 2:
        return [f"expected exactly 2 lane records (1-call and 10-call), got {len(records)}"]
    errors: list[str] = []
    for record, lane_calls in zip(records, CANONICAL_TOOL_CALL_LANES):
        lane_errs = validate_lane_record(
            record,
            expected_iterations=expected_iterations,
            expected_tool_calls=lane_calls,
            expected_run_id=expected_run_id,
            expected_correlation_id=expected_correlation_id,
            expected_binary_sha256=expected_binary_sha256,
            expected_allocator=expected_allocator,
            require_gate_eligible=require_gate_eligible,
        )
        for err in lane_errs:
            errors.append(f"lane(tool_calls={lane_calls}): {err}")
    if not errors:
        for field in SHARED_PAIR_FIELDS:
            if records[0].get(field) != records[1].get(field):
                errors.append(f"1-call and 10-call lanes disagree on {field!r}")
    return errors


def invoke_lane(
    binary: Path,
    *,
    workdir: Path,
    iterations: int,
    tool_calls: int,
    run_id: str,
    correlation_id: str,
    allocator: str,
) -> tuple[dict[str, Any] | None, str | None]:
    env = os.environ.copy()
    env["PI_BENCH_RUN_ID"] = run_id
    env["PI_BENCH_CORRELATION_ID"] = correlation_id
    env["PI_BENCH_ALLOCATOR"] = allocator
    try:
        proc = subprocess.run(
            [
                str(binary),
                "--iterations",
                str(iterations),
                "--tool-calls",
                str(tool_calls),
                "--runtime-engine",
                "quickjs",
            ],
            cwd=workdir,
            env=env,
            capture_output=True,
            text=True,
            timeout=600.0,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return None, f"pijs_workload (--tool-calls {tool_calls}) failed to run: {exc}"
    if proc.returncode != 0:
        return (
            None,
            f"pijs_workload (--tool-calls {tool_calls}) exited rc={proc.returncode}:\n{proc.stderr[-2000:]}",
        )
    lines = [line.strip() for line in proc.stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        return (
            None,
            f"pijs_workload (--tool-calls {tool_calls}) expected 1 JSON line, got {len(lines)}",
        )
    try:
        parsed = json.loads(lines[0])
    except json.JSONDecodeError as exc:
        return None, f"pijs_workload stdout is not valid JSON: {exc}"
    if not isinstance(parsed, dict):
        return None, "pijs_workload stdout JSON must be an object"
    return parsed, None


def write_jsonl_atomic(out_path: Path, records: list[dict[str, Any]]) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = out_path.with_name(f".{out_path.name}.tmp.{os.getpid()}")
    try:
        with tmp_path.open("w", encoding="utf-8") as handle:
            for record in records:
                handle.write(json.dumps(record, sort_keys=True) + "\n")
        tmp_path.replace(out_path)
    except Exception:
        tmp_path.unlink(missing_ok=True)
        raise


def run_self_test() -> int:
    old_target = os.environ.pop("CARGO_TARGET_DIR", None)
    try:
        with tempfile.TemporaryDirectory(prefix="pijs_workload_selftest_") as tmp:
            root = Path(tmp)
            out_path = root / "out/pijs_workload_perf.jsonl"

            # 1. Missing binary fails closed with exit 2 and writes nothing.
            rc = run_cli(["--workdir", str(root), "--out", str(out_path)])
            assert rc == 2, f"expected rc=2 for missing binary, got {rc}"
            assert not out_path.exists(), "missing binary must not write output artifact"

            # 2. Valid executable emitting canonical two-lane pi.perf.workload.v1 passes.
            fake_bin = root / "target/perf/examples/pijs_workload"
            fake_bin.parent.mkdir(parents=True, exist_ok=True)
            fake_bin.write_text(
                "#!/usr/bin/env python3\n"
                "import hashlib, json, os, sys\n"
                "args = sys.argv[1:]\n"
                "iters = int(args[args.index('--iterations') + 1])\n"
                "calls = int(args[args.index('--tool-calls') + 1])\n"
                "total = iters * calls\n"
                "elapsed_us = total * 45\n"
                "elapsed_us_f64 = float(elapsed_us) + 0.25\n"
                "eligible = os.environ.get('FORCE_INELIGIBLE', '0') != '1'\n"
                "digest = '0' * 64 if os.environ.get('TAMPER_SHA', '0') == '1' else hashlib.sha256(open(__file__, 'rb').read()).hexdigest()\n"
                "print(json.dumps({\n"
                "  'schema': 'pi.perf.workload.v1',\n"
                "  'timestamp': '2026-11-25T12:00:00.000Z',\n"
                "  'run_id': os.environ['PI_BENCH_RUN_ID'],\n"
                "  'correlation_id': os.environ['PI_BENCH_CORRELATION_ID'],\n"
                "  'source_commit': 'a' * 40,\n"
                "  'source_dirty': False,\n"
                "  'tool': 'pijs_workload',\n"
                "  'scenario': 'tool_call_roundtrip',\n"
                "  'iterations': iters,\n"
                "  'tool_calls_per_iteration': calls,\n"
                "  'total_calls': total,\n"
                "  'elapsed_ms': elapsed_us // 1000,\n"
                "  'elapsed_us': elapsed_us,\n"
                "  'elapsed_us_f64': elapsed_us_f64,\n"
                "  'per_call_us': elapsed_us // total,\n"
                "  'per_call_us_f64': elapsed_us_f64 / float(total),\n"
                "  'calls_per_sec': (total * 1_000_000) // elapsed_us,\n"
                "  'build_profile': 'perf',\n"
                "  'build_profile_verified': True,\n"
                "  'build_fingerprint_contract': 'cargo_build_fingerprint.v1',\n"
                "  'build_fingerprint_verified': True,\n"
                "  'executable_build_profile': 'perf',\n"
                "  'executable_profile_verified': True,\n"
                "  'compiled_profile_family': 'release',\n"
                "  'compiled_opt_level': '3',\n"
                "  'compiled_debug': 'true',\n"
                "  'compiled_features': ['clipboard', 'image', 'image-resize', 'sqlite-sessions', 'wasm-host'],\n"
                "  'debug_assertions': False,\n"
                "  'config_hash': '0' * 64 if os.environ.get('BAD_CONFIG_HASH', '0') == '1' else 'b' * 64,\n"
                "  'allocator_requested': os.environ['PI_BENCH_ALLOCATOR'],\n"
                "  'allocator_request_source': 'env',\n"
                "  'allocator_effective': os.environ['PI_BENCH_ALLOCATOR'],\n"
                "  'allocator_fallback_reason': None,\n"
                "  'runtime_engine': 'quickjs',\n"
                "  'evidence_class': 'measured',\n"
                "  'confidence': 'high' if eligible else 'medium',\n"
                "  'eligible_for_regression_gate': eligible,\n"
                "  'binary_path': __file__,\n"
                "  'binary_sha256': digest,\n"
                "}))\n",
                encoding="utf-8",
            )
            fake_bin.chmod(fake_bin.stat().st_mode | stat.S_IEXEC)

            rc = run_cli(
                [
                    "--workdir",
                    str(root),
                    "--binary",
                    str(fake_bin),
                    "--out",
                    str(out_path),
                    "--run-id",
                    "selftest-run-1",
                ]
            )
            assert rc == 0, f"expected rc=0 on canonical pair, got {rc}"
            lines = [json.loads(line) for line in out_path.read_text(encoding="utf-8").splitlines()]
            assert len(lines) == 2
            assert [row["tool_calls_per_iteration"] for row in lines] == [1, 10]
            assert all(row["schema"] == SCHEMA_RECORD for row in lines)
            assert all(row["binary_sha256"] == sha256_file(fake_bin) for row in lines)

            # 3. Ineligible record fails closed and does not write output.
            bad_out = root / "out/should_not_exist.jsonl"
            old_val = os.environ.get("FORCE_INELIGIBLE")
            os.environ["FORCE_INELIGIBLE"] = "1"
            try:
                rc = run_cli(
                    [
                        "--workdir",
                        str(root),
                        "--binary",
                        str(fake_bin),
                        "--out",
                        str(bad_out),
                        "--run-id",
                        "selftest-run-2",
                    ]
                )
            finally:
                if old_val is None:
                    os.environ.pop("FORCE_INELIGIBLE", None)
                else:
                    os.environ["FORCE_INELIGIBLE"] = old_val
            assert rc == 1, f"expected rc=1 for ineligible gate pair, got {rc}"
            assert not bad_out.exists(), "ineligible pair must not write artifact"

            # 4. Tampered binary_sha256 and invalid CLI flags fail closed.
            old_tamper = os.environ.get("TAMPER_SHA")
            os.environ["TAMPER_SHA"] = "1"
            try:
                rc = run_cli(
                    [
                        "--workdir",
                        str(root),
                        "--binary",
                        str(fake_bin),
                        "--out",
                        str(bad_out),
                        "--run-id",
                        "selftest-run-3",
                    ]
                )
            finally:
                if old_tamper is None:
                    os.environ.pop("TAMPER_SHA", None)
                else:
                    os.environ["TAMPER_SHA"] = old_tamper
            assert rc == 1, f"expected rc=1 for tampered binary_sha256, got {rc}"
            old_cfg_hash = os.environ.get("BAD_CONFIG_HASH")
            os.environ["BAD_CONFIG_HASH"] = "1"
            try:
                rc = run_cli(
                    [
                        "--workdir",
                        str(root),
                        "--binary",
                        str(fake_bin),
                        "--out",
                        str(bad_out),
                        "--run-id",
                        "selftest-run-4",
                    ]
                )
            finally:
                if old_cfg_hash is None:
                    os.environ.pop("BAD_CONFIG_HASH", None)
                else:
                    os.environ["BAD_CONFIG_HASH"] = old_cfg_hash
            assert rc == 1, f"expected rc=1 for zeroed config_hash, got {rc}"
            assert run_cli(["--binary", str(fake_bin), "--iterations", "0", "--out", str(bad_out)]) == 1
            assert run_cli(["--binary", str(fake_bin), "--iterations", "10", "--out", str(bad_out)]) == 1
            assert run_cli(["--binary", str(fake_bin), "--allocator", "jemalloc", "--out", str(bad_out)]) == 1
            assert run_cli(["--binary", str(fake_bin), "--run-id", "a", "--correlation-id", "b", "--out", str(bad_out)]) == 1
            assert run_cli(["--binary", str(fake_bin), "--run-id", "   ", "--out", str(bad_out)]) == 1
            assert not bad_out.exists()
    finally:
        if old_target is not None:
            os.environ["CARGO_TARGET_DIR"] = old_target

    print("run_pijs_workload.py --self-test: PASS")
    return 0


def run_cli(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--workdir", type=Path, default=project_root())
    ap.add_argument(
        "--binary",
        type=Path,
        default=None,
        help="Explicit path to perf-profile pijs_workload executable",
    )
    ap.add_argument("--iterations", type=int, default=CANONICAL_ITERATIONS)
    ap.add_argument(
        "--out",
        type=Path,
        default=project_root() / "tests/perf/reports/pijs_workload_perf.jsonl",
    )
    ap.add_argument("--run-id", default=None)
    ap.add_argument("--correlation-id", default=None)
    ap.add_argument("--allocator", default="system", choices=("system", "jemalloc"))
    ap.add_argument(
        "--allow-non-gate",
        action="store_true",
        help="Allow writing records whose eligible_for_regression_gate is false (diagnostic only)",
    )
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="Run self-contained contract tests in a temporary directory and exit",
    )
    args = ap.parse_args(argv)

    if args.self_test:
        return run_self_test()

    if args.iterations <= 0:
        print(f"FAIL: --iterations must be positive, got {args.iterations}", file=sys.stderr)
        return 1
    if not args.allow_non_gate and args.iterations != CANONICAL_ITERATIONS:
        print(
            f"FAIL: regression-gate runs require --iterations {CANONICAL_ITERATIONS} "
            f"(got {args.iterations}); pass --allow-non-gate for diagnostic runs",
            file=sys.stderr,
        )
        return 1
    if not args.allow_non_gate and args.allocator != "system":
        print(
            f"FAIL: regression-gate runs require --allocator system (got {args.allocator!r}); "
            "pass --allow-non-gate for diagnostic runs",
            file=sys.stderr,
        )
        return 1
    if args.run_id is not None and not args.run_id.strip():
        print("FAIL: --run-id must be non-empty when specified", file=sys.stderr)
        return 1
    if args.correlation_id is not None and not args.correlation_id.strip():
        print("FAIL: --correlation-id must be non-empty when specified", file=sys.stderr)
        return 1
    explicit_run_id = args.run_id.strip() if args.run_id is not None else None
    explicit_corr_id = args.correlation_id.strip() if args.correlation_id is not None else None
    if explicit_run_id and explicit_corr_id and explicit_run_id != explicit_corr_id:
        print(
            f"FAIL: --run-id ({explicit_run_id!r}) and --correlation-id ({explicit_corr_id!r}) must match",
            file=sys.stderr,
        )
        return 1

    binary = resolve_binary(args.workdir, args.binary)
    if binary is None:
        searched = ", ".join(str(p) for p in candidate_binaries(args.workdir, args.binary))
        print(
            f"FAIL: perf-profile pijs_workload binary not found (searched: {searched}); "
            "build it through the DSR/perf lane (`cargo build --locked --profile perf "
            "--no-default-features --features clipboard,image,image-resize,sqlite-sessions,wasm-host "
            "--example pijs_workload`) and rerun. No artifact written.",
            file=sys.stderr,
        )
        return 2

    default_id = f"pijs-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}"
    run_id = explicit_run_id or explicit_corr_id or default_id
    correlation_id = explicit_corr_id or run_id
    expected_binary_sha256 = sha256_file(binary)

    records: list[dict[str, Any]] = []
    for tool_calls in CANONICAL_TOOL_CALL_LANES:
        record, err = invoke_lane(
            binary,
            workdir=args.workdir,
            iterations=args.iterations,
            tool_calls=tool_calls,
            run_id=run_id,
            correlation_id=correlation_id,
            allocator=args.allocator,
        )
        if err is not None or record is None:
            print(f"FAIL: {err}", file=sys.stderr)
            return 1
        records.append(record)

    errors = validate_pair(
        records,
        expected_iterations=args.iterations,
        expected_run_id=run_id,
        expected_correlation_id=correlation_id,
        expected_binary_sha256=expected_binary_sha256,
        expected_allocator=args.allocator,
        require_gate_eligible=not args.allow_non_gate,
    )
    if errors:
        print("FAIL: pijs_workload pair failed contract validation (no artifact written):", file=sys.stderr)
        for err in errors:
            print(f"  - {err}", file=sys.stderr)
        return 1

    write_jsonl_atomic(args.out, records)
    lat_us = records[0]["per_call_us_f64"]
    tp_cps = records[1]["calls_per_sec"]
    print(
        f"wrote {args.out}: 2 canonical lanes (1-call & 10-call x {args.iterations}), "
        f"tool_call_latency_mean={lat_us:.2f}us, tool_call_throughput_min={tp_cps} calls/sec, "
        f"eligible_for_regression_gate={records[0]['eligible_for_regression_gate']}"
    )
    return 0


def main() -> int:
    return run_cli()


if __name__ == "__main__":
    sys.exit(main())
