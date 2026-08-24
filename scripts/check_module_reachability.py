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
ALLOWLIST: dict[str, str] = {
    "conformance_shapes": (
        "Shape-aware conformance harness for extension types. A test-suite "
        "fixture engine by construction; its consumer is "
        "tests/ext_conformance_shapes.rs and there is no product path that "
        "should call it."
    ),
    "flake_classifier": (
        "Classifies test failures as deterministic vs transient for CI retry "
        "and triage (bd-k5q5.5.4). Consumed by tests/provider_native_contract.rs "
        "and CI tooling; the shipped agent has no reason to classify its own "
        "test flakes."
    ),
    "swarm_flight_recorder": (
        "Deterministic E2E evidence harness for multi-agent runs, documented in "
        "docs/swarm-flight-recorder.md as driven via "
        "`cargo test --test e2e_swarm_flight_recorder`. Test-lane by design -- "
        "it consumes already-emitted runtime events rather than being called "
        "from them."
    ),
}

# `pub mod foo;` -- declarations only. `pub mod foo { ... }` inline modules are
# not separate files and are not what this gate is about.
PUB_MOD_RE = re.compile(r"^\s*pub mod\s+([a-z_][a-z0-9_]*)\s*;", re.MULTILINE)

# `#[cfg(test)]` immediately preceding a `mod ... {`. Only a test *module* is
# skipped wholesale; a bare `#[cfg(test)]` on a single field or fn says nothing
# about the code after it.
CFG_TEST_RE = re.compile(r"^\s*#\[cfg\(test\)\]\s*$")
MOD_OPEN_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[a-z_][a-z0-9_]*\s*\{")

# Rust tokens that can carry an unbalanced brace. Stripped before depth
# counting so a `"}"` inside a string cannot close a real block.
STRING_LIKE_RE = re.compile(
    r"""
      r\#*"(?:[^"\\]|\\.)*"\#*   # raw string (approximate: no embedded quote+hash)
    | "(?:[^"\\]|\\.)*"          # normal string
    | '(?:[^'\\]|\\.)'           # char literal
    | //.*$                      # line comment
    """,
    re.VERBOSE,
)


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


def referencing_lines(roots: list[Path], name: str) -> list[tuple[Path, int, str]]:
    """Every `<name>::` occurrence under the given roots, as (path, line, text).

    Deliberately textual rather than syntactic: the question is "does any other
    part of the crate reach for this module", and a grep answers it without a
    parse. Over-matching is safe here -- a false *pass* needs a real mention of
    the module somewhere in shipped code, which is the signal we want.
    """
    hits: list[tuple[Path, int, str]] = []
    pattern = re.compile(rf"\b{re.escape(name)}::")
    for root in roots:
        if not root.is_dir():
            continue
        for path in root.rglob("*.rs"):
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


def test_module_lines(path: Path) -> set[int]:
    """1-indexed line numbers inside a `#[cfg(test)] mod ... { }` block.

    Brace-tracked rather than proximity-guessed. The naive version of this --
    "was there a test marker in the preceding N lines" -- misfires badly on real
    source: src/interactive/view.rs carries a bare `#[cfg(test)]` on a single
    item at line 707, which would falsely mark the next several hundred lines of
    production rendering code as test-only.

    Braces inside strings, char literals, and line comments are stripped before
    counting so a `"}"` in a message cannot close a block early. Block comments
    are not stripped; an unbalanced brace inside one would need to be written
    deliberately, and the failure mode is a module reported as reachable, which
    is the safe direction for a gate.
    """
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError):
        return set()

    inside: set[int] = set()
    depth = 0
    pending_cfg_test = False
    block_depth: int | None = None

    for idx, raw in enumerate(lines, start=1):
        line = STRING_LIKE_RE.sub("", raw)

        if block_depth is None and pending_cfg_test and MOD_OPEN_RE.match(line):
            # Enter: this line's opening brace puts us at block_depth + 1.
            block_depth = depth

        if block_depth is not None:
            inside.add(idx)

        depth += line.count("{") - line.count("}")

        if block_depth is not None and depth <= block_depth:
            block_depth = None  # matching close brace seen

        # A `#[cfg(test)]` arms only the declaration that directly follows it.
        if CFG_TEST_RE.match(raw):
            pending_cfg_test = True
        elif line.strip() and not line.lstrip().startswith("#["):
            pending_cfg_test = False

    return inside


def classify(root: Path, name: str) -> tuple[str, list[str]]:
    """Return (verdict, evidence) for one module.

    Verdict is `reachable`, `test_only`, or `unreferenced`.

    `examples/` and `benches/` count alongside `src/`: in this repo they are
    real entry points, not scaffolding -- the perf and conformance tooling
    genuinely runs via `cargo run --example`. `tests/` deliberately does not
    count, because "a test pokes it" is precisely the state this gate exists to
    distinguish from "a user can reach it".
    """
    src = root / "src"
    roots = [src, root / "examples", root / "benches"]
    owned = module_files(src, name)
    evidence: list[str] = []
    saw_test_only = False
    test_lines: dict[Path, set[int]] = {}

    for path, lineno, text in referencing_lines(roots, name):
        if path in owned:
            continue
        try:
            rel = path.relative_to(root)
        except ValueError:
            rel = path
        if path not in test_lines:
            test_lines[path] = test_module_lines(path)
        if lineno in test_lines[path]:
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
        verdict, _evidence = classify(root, name)
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
