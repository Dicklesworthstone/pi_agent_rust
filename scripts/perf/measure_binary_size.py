#!/usr/bin/env python3
"""scripts/perf/measure_binary_size.py

Canonical release binary-size measurement control
(`src/perf_build.rs::BinarySizeMeasurementControl`, `#[serde(deny_unknown_fields)]`).

Inspects an existing release `pi` binary (or builds with `--build` when
explicitly requested), verifies that the binary is stripped, and atomically
writes the exact 15-field `pi.perf.binary_size_measurement.v1` control artifact
consumed by `src/perf_build.rs::verify_binary_size_measurement_control`.

Exit 0 = artifact written, schema valid, binary stripped and within budget.
Exit 1 = binary missing, unstripped, build failed, or over budget.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = "pi.perf.binary_size_measurement.v1"
REQUIRED_FIELDS = (
    "schema",
    "generated_at",
    "run_id",
    "correlation_id",
    "source_commit",
    "source_dirty",
    "binary_path",
    "binary_sha256",
    "size_bytes",
    "cargo_profile",
    "compiled_profile_family",
    "compiled_opt_level",
    "strip",
    "profile_source",
    "build_command",
)


def project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def git_source_identity(workdir: Path) -> tuple[str, bool]:
    commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=workdir, text=True
    ).strip()
    dirty = bool(
        subprocess.check_output(
            ["git", "status", "--porcelain"], cwd=workdir, text=True
        ).strip()
    )
    return commit, dirty


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def is_stripped(path: Path) -> bool:
    """Return True only when ELF/Mach-O symbol inspection confirms stripped."""
    if not path.is_file():
        return False
    nm = shutil.which("nm")
    if nm is not None:
        try:
            proc = subprocess.run(
                [nm, "-p", str(path)],
                capture_output=True,
                text=True,
                timeout=5.0,
            )
            combined = f"{proc.stdout}\n{proc.stderr}".lower()
            if "no symbols" in combined:
                return True
            if proc.returncode == 0:
                return not any(line.strip() for line in proc.stdout.splitlines())
        except (OSError, subprocess.TimeoutExpired):
            pass
    file_bin = shutil.which("file")
    if file_bin is not None:
        try:
            proc = subprocess.run(
                [file_bin, str(path)],
                capture_output=True,
                text=True,
                timeout=5.0,
            )
            out = proc.stdout.lower()
            return proc.returncode == 0 and "stripped" in out and "not stripped" not in out
        except (OSError, subprocess.TimeoutExpired):
            return False
    return False


def is_lowercase_nonzero_git_sha(raw: str) -> bool:
    return (
        len(raw) == 40
        and all(ch in "0123456789abcdef" for ch in raw)
        and not all(ch == "0" for ch in raw)
    )


def default_release_binary(workdir: Path, *, no_build: bool = True) -> Path:
    env_target = os.environ.get("CARGO_TARGET_DIR", "").strip()
    if env_target:
        target_path = Path(env_target)
        if not target_path.is_absolute():
            target_path = workdir / target_path
        cand = target_path / "release/pi"
        if not no_build or cand.exists():
            return cand
    return workdir / "target/release/pi"


def run_self_test() -> int:
    old_target = os.environ.pop("CARGO_TARGET_DIR", None)
    try:
        with tempfile.TemporaryDirectory(prefix="measure_binary_size_selftest_") as tmp:
            root = Path(tmp)
            out_path = root / "binary_size_measurement.json"

            # 1. Missing binary fails closed without writing a bogus 0-byte artifact.
            rc = run_cli(
                [
                    "--workdir",
                    str(root),
                    "--no-build",
                    "--out",
                    str(out_path),
                    "--source-commit",
                    "a" * 40,
                ]
            )
            assert rc == 1, f"expected rc=1 for missing binary, got {rc}"
            assert not out_path.exists()

            # 2. Validate exact 15-field deny_unknown_fields schema against a real stripped binary if present.
            sample_bin = Path("/bin/true")
            if sample_bin.is_file():
                release_dir = root / "target/release"
                release_dir.mkdir(parents=True, exist_ok=True)
                copied = release_dir / "pi"
                shutil.copy2(sample_bin, copied)
                rc = run_cli(
                    [
                        "--workdir",
                        str(root),
                        "--binary",
                        str(copied),
                        "--no-build",
                        "--out",
                        str(out_path),
                        "--run-id",
                        "selftest-bin-1",
                        "--source-commit",
                        "a" * 40,
                        "--source-clean",
                    ]
                )
                if is_stripped(copied):
                    assert rc == 0, f"expected rc=0 for stripped binary, got {rc}"
                    payload = json.loads(out_path.read_text(encoding="utf-8"))
                    assert tuple(sorted(payload.keys())) == tuple(sorted(REQUIRED_FIELDS))
                    assert payload["schema"] == SCHEMA
                    assert payload["profile_source"] == "Cargo.toml#profile.release"
                    assert payload["build_command"] == "cargo build --bin pi --release"
                    assert payload["binary_sha256"] == sha256_file(copied)
                    assert payload["size_bytes"] == copied.stat().st_size

                # 3. Negative CLI/lineage controls fail closed and write nothing.
                neg_out = root / "neg_binary_size.json"
                assert run_cli(["--binary", str(copied), "--no-build", "--threshold-mb", "0", "--out", str(neg_out), "--source-commit", "a" * 40]) == 1
                assert run_cli(["--binary", str(copied), "--no-build", "--threshold-mb", "nan", "--out", str(neg_out), "--source-commit", "a" * 40]) == 1
                assert run_cli(["--binary", str(copied), "--no-build", "--run-id", "a", "--correlation-id", "b", "--out", str(neg_out), "--source-commit", "a" * 40]) == 1
                assert run_cli(["--binary", str(copied), "--no-build", "--out", str(neg_out), "--source-commit", "0" * 40]) == 1
                assert not neg_out.exists()
    finally:
        if old_target is not None:
            os.environ["CARGO_TARGET_DIR"] = old_target

    print("measure_binary_size.py --self-test: PASS")
    return 0


def run_cli(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--workdir", type=Path, default=project_root())
    ap.add_argument("--binary", type=Path, default=None)
    ap.add_argument(
        "--out",
        type=Path,
        default=project_root() / "tests/perf/reports/release_evidence/binary_size_measurement.json",
    )
    ap.add_argument("--threshold-mb", type=float, default=48.0)
    ap.add_argument("--run-id", default=None)
    ap.add_argument("--correlation-id", default=None)
    ap.add_argument("--source-commit", default=None)
    ap.add_argument("--source-clean", action="store_true", help="Override source_dirty=False (tests only)")
    ap.add_argument(
        "--no-build",
        action="store_true",
        help="Inspect existing release/pi binary without invoking cargo",
    )
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)

    if args.self_test:
        return run_self_test()

    if not math.isfinite(args.threshold_mb) or args.threshold_mb <= 0.0:
        print(f"FAIL: --threshold-mb must be finite and > 0, got {args.threshold_mb}", file=sys.stderr)
        return 1
    if args.run_id and args.correlation_id and args.run_id != args.correlation_id:
        print(
            f"FAIL: --run-id ({args.run_id!r}) and --correlation-id ({args.correlation_id!r}) must match",
            file=sys.stderr,
        )
        return 1

    binary = (args.binary or default_release_binary(args.workdir, no_build=args.no_build)).resolve()
    if not args.no_build:
        print(f"building {binary} with cargo build --locked --bin pi --release...", file=sys.stderr)
        try:
            proc = subprocess.run(
                ["cargo", "build", "--locked", "--bin", "pi", "--release"],
                cwd=args.workdir,
                capture_output=True,
                text=True,
            )
        except OSError as exc:
            print(f"FAIL: could not invoke cargo: {exc} (no artifact written)", file=sys.stderr)
            return 1
        if proc.returncode != 0:
            print(
                f"FAIL: cargo build failed (rc={proc.returncode}); no artifact written:\n{proc.stderr[-2000:]}",
                file=sys.stderr,
            )
            return 1

    if not binary.is_file():
        print(f"FAIL: release binary not found: {binary} (no artifact written)", file=sys.stderr)
        return 1

    if args.source_commit is not None:
        source_commit = args.source_commit.strip().lower()
        source_dirty = not args.source_clean
    else:
        try:
            source_commit, source_dirty = git_source_identity(args.workdir)
            source_commit = source_commit.lower()
        except (OSError, subprocess.CalledProcessError) as exc:
            print(f"FAIL: could not determine git source identity in {args.workdir}: {exc}", file=sys.stderr)
            return 1
    if not is_lowercase_nonzero_git_sha(source_commit):
        print(
            f"FAIL: source_commit must be a 40-char lowercase non-zero git SHA, got {source_commit!r}",
            file=sys.stderr,
        )
        return 1
    if source_dirty:
        print(
            "NOTE: source_dirty=true (diagnostic measurement; strict release-budget gate requires clean HEAD)",
            file=sys.stderr,
        )
    if binary.parent.name != "release" or binary.name != "pi":
        print(
            f"NOTE: binary_path={binary} does not end with release/pi "
            "(strict verify_binary_size_measurement_control requires a canonical release/pi path)",
            file=sys.stderr,
        )

    default_id = f"binary-size-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}"
    run_id = args.run_id or args.correlation_id or default_id
    correlation_id = args.correlation_id or run_id

    size_bytes = binary.stat().st_size
    size_mb = size_bytes / (1024.0 * 1024.0)
    stripped = is_stripped(binary)
    if not stripped:
        print(
            f"FAIL: {binary} is not stripped (verify_binary_size_measurement_control requires strip=true); "
            "no artifact written.",
            file=sys.stderr,
        )
        return 1

    artifact = {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
        "run_id": run_id,
        "correlation_id": correlation_id,
        "source_commit": source_commit,
        "source_dirty": source_dirty,
        "binary_path": str(binary),
        "binary_sha256": sha256_file(binary),
        "size_bytes": size_bytes,
        "cargo_profile": "release",
        "compiled_profile_family": "release",
        "compiled_opt_level": "3",
        "strip": True,
        "profile_source": "Cargo.toml#profile.release",
        "build_command": "cargo build --bin pi --release",
    }
    assert tuple(sorted(artifact.keys())) == tuple(sorted(REQUIRED_FIELDS))

    args.out.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = args.out.with_name(f".{args.out.name}.tmp.{os.getpid()}")
    try:
        tmp_path.write_text(json.dumps(artifact, indent=2) + "\n", encoding="utf-8")
        tmp_path.replace(args.out)
    except Exception:
        tmp_path.unlink(missing_ok=True)
        raise

    within_budget = size_mb <= args.threshold_mb
    status = "PASS" if within_budget else "OVER_BUDGET"
    print(
        f"wrote {args.out}: {binary.name} size={size_mb:.2f} MiB ({size_bytes} bytes) "
        f"stripped=True status={status} (threshold {args.threshold_mb} MiB)"
    )
    return 0 if within_budget else 1


def main() -> int:
    return run_cli()


if __name__ == "__main__":
    sys.exit(main())
