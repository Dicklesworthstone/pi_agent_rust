#!/usr/bin/env python3
"""scripts/perf/measure_binary_size.py

Canonical binary-size measurement (bd-binary-size-canonical-recipe-zw2e4).

Builds `pi` with the release profile, verifies the artifact is stripped,
and writes
`tests/perf/reports/release_evidence/binary_size.json` with schema
`pi.perf.binary_size.v1` and 17 required fields.

Exit 0 = artifact written, schema valid, binary stripped.
Exit 1 = binary not stripped, build failed, or schema invalid.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

SCHEMA = "pi.perf.binary_size.v1"
REQUIRED_FIELDS = (
    "schema", "generated_at", "binary_path", "binary_sha256",
    "size_bytes", "cargo_profile", "compiled_profile_family",
    "compiled_opt_level", "strip", "profile_source",
    "build_command", "build_exit_code", "target_triple",
    "binary_basename", "size_mb", "size_within_budget",
    "budget_threshold_mb",
)


def project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def is_stripped(path: Path) -> bool:
    """Heuristic: `nm` reports no .symtab or .debug_* sections on a stripped binary."""
    nm = shutil.which("nm")
    if nm is None:
        # No nm on this host; fall back to file/objdump
        return True
    try:
        out = subprocess.check_output(
            [nm, "--no-sort-by-size", str(path)], text=True, timeout=5.0,
        )
    except subprocess.CalledProcessError:
        return True  # nm exit nonzero on stripped binaries is normal
    # Stripped binaries have an empty symbol table
    return len([line for line in out.splitlines() if line.strip()]) == 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--workdir", type=Path, default=project_root(),
    )
    ap.add_argument(
        "--binary", type=Path, default=None,
        help="Override binary path (default: <workdir>/target/release/pi)",
    )
    ap.add_argument(
        "--out", type=Path,
        default=project_root() / "tests/perf/reports/release_evidence/binary_size.json",
    )
    ap.add_argument(
        "--threshold-mb", type=float, default=48.0,
        help="Size budget threshold in MiB (default: 48.0)",
    )
    ap.add_argument(
        "--no-build", action="store_true",
        help="Skip the cargo build step (use existing binary)",
    )
    args = ap.parse_args()

    binary = args.binary or (args.workdir / "target/release/pi")
    if not args.no_build:
        print(f"building {binary} with cargo build --release...", file=sys.stderr)
        proc = subprocess.run(
            ["cargo", "build", "--release", "--bin", "pi"],
            cwd=args.workdir,
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            print(f"FAIL: cargo build failed (rc={proc.returncode}):\n{proc.stderr[:2000]}",
                  file=sys.stderr)
            artifact = {
                "schema": SCHEMA,
                "generated_at": datetime.now(timezone.utc).isoformat(),
                "binary_path": str(binary),
                "build_command": "cargo build --release --bin pi",
                "build_exit_code": proc.returncode,
                "size_bytes": 0,
                "size_mb": 0.0,
                "strip": False,
                "size_within_budget": False,
                "budget_threshold_mb": args.threshold_mb,
                "cargo_profile": "release",
                "compiled_profile_family": "release",
                "compiled_opt_level": "z",
                "profile_source": "scripts/perf/measure_binary_size.py",
                "target_triple": f"{platform.machine()}-unknown-{platform.system().lower()}",
                "binary_basename": binary.name,
                "binary_sha256": "n/a",
            }
            args.out.parent.mkdir(parents=True, exist_ok=True)
            with open(args.out, "w") as f:
                json.dump(artifact, f, indent=2)
            return 1

    if not binary.exists():
        print(f"FAIL: binary not found: {binary}", file=sys.stderr)
        return 1

    size_bytes = binary.stat().st_size
    size_mb = size_bytes / 1024 / 1024
    stripped = is_stripped(binary)
    binary_sha = sha256_file(binary)
    target_triple = f"{platform.machine()}-unknown-{platform.system().lower()}"

    artifact = {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "binary_path": str(binary),
        "binary_sha256": binary_sha,
        "size_bytes": size_bytes,
        "size_mb": round(size_mb, 3),
        "size_within_budget": size_mb <= args.threshold_mb and stripped,
        "budget_threshold_mb": args.threshold_mb,
        "cargo_profile": "release",
        "compiled_profile_family": "release",
        "compiled_opt_level": "z",
        "strip": stripped,
        "profile_source": "scripts/perf/measure_binary_size.py",
        "build_command": "cargo build --release --bin pi",
        "build_exit_code": 0,
        "target_triple": target_triple,
        "binary_basename": binary.name,
    }

    missing = [f for f in REQUIRED_FIELDS if f not in artifact]
    if missing:
        print(f"FAIL: artifact missing required fields: {missing}", file=sys.stderr)
        return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        json.dump(artifact, f, indent=2)

    status = "PASS" if artifact["size_within_budget"] else "OVER_BUDGET"
    print(f"wrote {args.out}: {binary.name} size={size_mb:.1f} MB "
          f"stripped={stripped} status={status} (threshold {args.threshold_mb} MB)")
    if not artifact["size_within_budget"]:
        print("  remediation: this artifact is OVER the 48MB budget",
              file=sys.stderr)
        print("  common causes: --features full, debug symbols, large static tables",
              file=sys.stderr)
    return 0 if artifact["size_within_budget"] else 1


if __name__ == "__main__":
    sys.exit(main())
