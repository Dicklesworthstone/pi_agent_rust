#!/usr/bin/env python3
"""Vendor & verify the ext-conformance artifact corpus (bd-sog97.29).

The must-pass gate (`cargo test --test ext_conformance_generated --features
ext-conformance -- conformance_must_pass_gate`) loads extension sources from
`tests/ext_conformance/artifacts/`. Those files are gitignored by default
(`tests/ext_conformance/artifacts/*`), which made historical green runs
depend on untracked machine-local state: a pristine checkout yielded
143/208 with empty-observation failures.

Fix: vendor exactly the closure the manifest needs (each VALIDATED_MANIFEST
entry_path plus its relative-import dependencies, node_modules excluded) and
track it with a SHA-256 manifest.

Usage:
    scripts/vendor_ext_conformance_artifacts.py --verify       # CI / clean checkout
    scripts/vendor_ext_conformance_artifacts.py --regenerate   # maintainer: rewrite sums

`--verify` fails when any manifest entry or vendored dependency is missing
from the working tree or hashes differently than recorded. Files are added
with `git add -f` by the maintainer during regeneration (gitignore does not
untrack already-tracked paths).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / "tests" / "ext_conformance" / "artifacts"
MANIFEST = ROOT / "tests" / "ext_conformance" / "VALIDATED_MANIFEST.json"
SUMS = ARTIFACTS / "VENDORED_SHA256SUMS.txt"

IMPORT_RE = re.compile(
    r"""(?:from\s+|import\s*\(?\s*|require\s*\(\s*)['"](\.[^'"]+)['"]"""
)


def load_entry_paths() -> list[str]:
    data = json.loads(MANIFEST.read_text())
    return [e["entry_path"] for e in data["extensions"]]


def closure_for(entry_file: Path, needed: set[str]) -> None:
    rel = entry_file.relative_to(ARTIFACTS).as_posix()
    if rel in needed:
        return
    needed.add(rel)
    try:
        text = entry_file.read_text(errors="ignore")
    except OSError:
        return
    for match in IMPORT_RE.finditer(text):
        target = (entry_file.parent / match.group(1)).resolve()
        if ARTIFACTS.resolve() not in target.parents:
            continue
        candidates = (
            target,
            Path(f"{target}.ts"),
            target / "index.ts",
            Path(f"{target}.js"),
            target / "index.js",
        )
        for cand in candidates:
            if cand.is_file() and "node_modules" not in cand.parts:
                closure_for(cand, needed)
                break


def compute_closure() -> tuple[set[str], list[str]]:
    needed: set[str] = set()
    missing: list[str] = []
    for entry in sorted(load_entry_paths()):
        entry_file = ARTIFACTS / entry
        if not entry_file.is_file():
            missing.append(entry)
            continue
        closure_for(entry_file, needed)
    return needed, missing


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def cmd_verify() -> int:
    needed, missing_entries = compute_closure()
    failures: list[str] = []

    recorded: dict[str, str] = {}
    for line in SUMS.read_text().splitlines():
        if line.strip():
            sha, _, name = line.partition("  ")
            recorded[name] = sha

    for entry in missing_entries:
        failures.append(f"manifest entry absent on disk: {entry}")

    for name in sorted(needed):
        path = ARTIFACTS / name
        if not path.is_file():
            failures.append(f"vendored file missing: {name}")
            continue
        actual = sha256(path)
        expected = recorded.get(name)
        if expected is None:
            failures.append(f"not in {SUMS.name}: {name}")
        elif expected != actual:
            failures.append(f"hash mismatch: {name}")

    for name in sorted(set(recorded) - needed):
        failures.append(f"stale sums entry (not in closure): {name}")

    if failures:
        print(f"[vendor] VERIFY FAILED ({len(failures)} problems):")
        for problem in failures[:50]:
            print(f"  - {problem}")
        if len(failures) > 50:
            print(f"  ... and {len(failures) - 50} more")
        print("[vendor] regenerate with: scripts/vendor_ext_conformance_artifacts.py --regenerate")
        return 1
    print(f"[vendor] OK: {len(needed)} files verified against {SUMS.name}")
    return 0


def cmd_regenerate() -> int:
    needed, missing_entries = compute_closure()
    if missing_entries:
        print("[vendor] cannot regenerate: missing manifest entries:")
        for entry in missing_entries:
            print(f"  - {entry}")
        return 1
    lines = [f"{sha256(ARTIFACTS / name)}  {name}" for name in sorted(needed)]
    SUMS.write_text("\n".join(lines) + "\n")
    print(f"[vendor] wrote {len(lines)} sums to {SUMS}")
    print('[vendor] next: git add -f the listed files, then commit them with the sums')
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--verify", action="store_true")
    group.add_argument("--regenerate", action="store_true")
    args = parser.parse_args(argv)
    return cmd_verify() if args.verify else cmd_regenerate()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
