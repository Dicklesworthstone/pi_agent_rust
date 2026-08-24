#!/usr/bin/env python3
"""Fail when a `pub mod` in src/lib.rs has no non-test call site in src/.

AGENTS.md makes ledger reconciliation a pre-commit invariant to stop "completion
illusion where all beads appear closed but critical gaps remain untracked". On
2026-08-24 that illusion happened anyway (bd-33df9): five modules landed with
green unit tests, their beads were closed, and nothing in the product ever
called them. `scripts/reconcile_beads_ledger.sh` exited 0 throughout, because it
only cross-references the parity gap ledger and structurally cannot see a bead
closed against code that compiles, tests clean, and is unreachable.

This gate closes that class. A module that only its own tests reference is not a
shipped feature, and saying so out loud is cheap.

Library-only modules are legitimate -- the SDK deliberately exposes surface no
internal caller uses -- so they are declared in ALLOWLIST with a reason instead
of being silently tolerated. The reason string is the point: it converts "nobody
noticed" into "someone decided".

Exit 0 = every module is reachable or explicitly allowlisted.
Exit 1 = at least one module is unreachable and undeclared.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

# Modules with no internal caller by design. Each entry must say why, because a
# reason is what distinguishes a decision from an oversight.
#
# Do NOT add a module here to make the gate quiet. If a feature is supposed to
# be reachable and is not, the fix is a call site or a bead -- not an entry.
ALLOWLIST: dict[str, str] = {}

# `pub mod foo;` -- declarations only. `pub mod foo { ... }` inline modules are
# not separate files and are not what this gate is about.
PUB_MOD_RE = re.compile(r"^\s*pub mod\s+([a-z_][a-z0-9_]*)\s*;", re.MULTILINE)

# A cfg(test) module or a #[test] fn inside src/ still counts as a test-only
# reference: the feature is not reachable from a user action just because a unit
# test pokes it.
TEST_MARKER_RE = re.compile(r"#\[cfg\(test\)\]|#\[test\]|mod tests")


def repo_root() -> Path:
    """Repository root, so the gate works from any working directory."""
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=True,
        )
        return Path(out.stdout.strip())
    except (subprocess.CalledProcessError, FileNotFoundError):
        # Not a git checkout (vendored tarball, container): fall back to the
        # script's own parent rather than failing the build for it.
        return Path(__file__).resolve().parent.parent


def declared_modules(lib_rs: Path) -> list[str]:
    return PUB_MOD_RE.findall(lib_rs.read_text(encoding="utf-8"))


def module_files(src: Path, name: str) -> set[Path]:
    """A module's own files: `src/foo.rs` plus everything under `src/foo/`.

    References from inside a module to itself never prove reachability.
    """
    owned = set()
    flat = src / f"{name}.rs"
    if flat.is_file():
        owned.add(flat.resolve())
    directory = src / name
    if directory.is_dir():
        owned.update(p.resolve() for p in directory.rglob("*.rs"))
    return owned


def referencing_lines(src: Path, name: str) -> list[tuple[Path, int, str]]:
    """Every `<name>::` occurrence under src/, as (path, line number, text).

    Deliberately textual rather than syntactic: the question is "does any other
    part of the crate reach for this module", and a grep answers it without a
    parse. Over-matching is safe here -- a false *pass* needs a real mention of
    the module somewhere in src/, which is already the signal we want.
    """
    hits: list[tuple[Path, int, str]] = []
    pattern = re.compile(rf"\b{re.escape(name)}::")
    for path in src.rglob("*.rs"):
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        if not pattern.search(text):
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            if pattern.search(line):
                hits.append((path.resolve(), lineno, line.strip()))
    return hits


def is_test_context(path: Path, lineno: int) -> bool:
    """Whether a hit sits in test-only code.

    Approximated by scanning the 400 lines above it for a test marker. Rust
    convention puts `#[cfg(test)] mod tests` at the end of a file, so a hit
    below such a marker is inside it. The window bounds the cost on very large
    files; a hit further than that from any marker is production code.
    """
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError):
        return False
    start = max(0, lineno - 400)
    return bool(TEST_MARKER_RE.search("\n".join(lines[start:lineno])))


def classify(src: Path, name: str) -> tuple[str, list[str]]:
    """Return (verdict, evidence) for one module.

    Verdict is `reachable`, `test_only`, or `unreferenced`.
    """
    owned = module_files(src, name)
    evidence: list[str] = []
    saw_test_only = False

    for path, lineno, text in referencing_lines(src, name):
        if path in owned:
            continue
        rel = path.relative_to(src.parent)
        if is_test_context(path, lineno):
            saw_test_only = True
            continue
        evidence.append(f"{rel}:{lineno}: {text[:100]}")
        if len(evidence) >= 3:
            break

    if evidence:
        return "reachable", evidence
    return ("test_only" if saw_test_only else "unreferenced"), []


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit a machine-readable report on stdout instead of prose",
    )
    args = parser.parse_args()

    root = repo_root()
    src = root / "src"
    lib_rs = src / "lib.rs"
    if not lib_rs.is_file():
        print(f"error: {lib_rs} not found", file=sys.stderr)
        return 1

    modules = declared_modules(lib_rs)
    if not modules:
        print(f"error: no `pub mod` declarations found in {lib_rs}", file=sys.stderr)
        return 1

    reachable: list[str] = []
    allowlisted: list[str] = []
    failures: list[tuple[str, str]] = []

    for name in sorted(modules):
        verdict, _evidence = classify(src, name)
        if verdict == "reachable":
            reachable.append(name)
        elif name in ALLOWLIST:
            allowlisted.append(name)
        else:
            failures.append((name, verdict))

    if args.json:
        print(
            json.dumps(
                {
                    "schema": "pi.ci.module_reachability.v1",
                    "declared": len(modules),
                    "reachable": reachable,
                    "allowlisted": {n: ALLOWLIST[n] for n in allowlisted},
                    "failures": [
                        {"module": n, "verdict": v} for n, v in failures
                    ],
                    "verdict": "fail" if failures else "pass",
                },
                indent=2,
            )
        )
        return 1 if failures else 0

    print(
        f"Module reachability: {len(modules)} declared, {len(reachable)} reachable, "
        f"{len(allowlisted)} allowlisted, {len(failures)} unreachable."
    )
    if not failures:
        return 0

    print("", file=sys.stderr)
    print(
        "UNREACHABLE MODULES: declared `pub mod` in src/lib.rs with no non-test",
        file=sys.stderr,
    )
    print("call site anywhere in src/.", file=sys.stderr)
    for name, verdict in failures:
        detail = (
            "only its own tests reference it"
            if verdict == "test_only"
            else "nothing references it"
        )
        print(f"  - {name}: {detail}", file=sys.stderr)
    print("", file=sys.stderr)
    print("A module no shipped code calls is not a shipped feature. Either:", file=sys.stderr)
    print("  1. land the call site that makes it reachable, or", file=sys.stderr)
    print("  2. add it to ALLOWLIST in this script with a real reason.", file=sys.stderr)
    print("", file=sys.stderr)
    print(
        "Do NOT delete a module to satisfy this gate -- AGENTS.md Rule 1 forbids",
        file=sys.stderr,
    )
    print("file deletion without the owner's written permission.", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
