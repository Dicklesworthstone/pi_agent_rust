#!/usr/bin/env python3
"""Mechanical checker for clean release source commits (RI-DROPIP, bd-sog97.16).

Verifies that the diff between a release baseline and target commit touches
ONLY approved evidence, report, documentation, and beads tracking paths,
ensuring that release evidence and verdicts reflect a frozen, unmutated
codebase.
"""

import argparse
import fnmatch
import json
import os
import subprocess
import sys

ALLOWED_PATTERNS = [
    "docs/evidence/*",
    "docs/contracts/*",
    "docs/dropin-*.json",
    "docs/dropin-*.md",
    "docs/parity-certification.json",
    "docs/releasing.md",
    "docs/qa-runbook.md",
    "docs/testing-policy.md",
    "tests/perf/reports/*",
    "tests/full_suite_gate/*",
    "tests/ext_conformance/reports/*",
    "tests/ext_conformance/reports/gate/*",
    ".beads/*",
    "CHANGELOG.md",
    "README.md",
    "release_readiness_report.md",
]

def is_path_allowed(path: str) -> bool:
    for pattern in ALLOWED_PATTERNS:
        if fnmatch.fnmatch(path, pattern) or fnmatch.fnmatch(path, pattern + "/**"):
            return True
        if path.startswith(pattern.rstrip("*")):
            return True
    return False

def check_clean_release_commit(base_ref: str, target_ref: str = "HEAD") -> dict:
    cmd = ["git", "diff", "--name-only", f"{base_ref}..{target_ref}"]
    try:
        res = subprocess.run(cmd, capture_output=True, text=True, check=True)
    except subprocess.CalledProcessError as e:
        return {
            "schema": "pi.release.clean_commit_check.v1",
            "base_ref": base_ref,
            "target_ref": target_ref,
            "error": f"git diff failed: {e.stderr}",
            "is_clean_release_commit": False,
            "modified_files": [],
            "violating_files": [],
            "allowed_files": [],
        }

    changed = [line.strip() for line in res.stdout.splitlines() if line.strip()]
    allowed = []
    violating = []

    for path in changed:
        if is_path_allowed(path):
            allowed.append(path)
        else:
            violating.append(path)

    is_clean = len(violating) == 0

    return {
        "schema": "pi.release.clean_commit_check.v1",
        "base_ref": base_ref,
        "target_ref": target_ref,
        "is_clean_release_commit": is_clean,
        "total_changed": len(changed),
        "allowed_count": len(allowed),
        "violating_count": len(violating),
        "allowed_files": allowed,
        "violating_files": violating,
    }

def main():
    parser = argparse.ArgumentParser(description="Check if commit range touches only release evidence")
    parser.add_argument("base_ref", nargs="?", default="HEAD~1", help="Base git ref or tag (default: HEAD~1)")
    parser.add_argument("target_ref", nargs="?", default="HEAD", help="Target git ref (default: HEAD)")
    parser.add_argument("--json", action="store_true", help="Output JSON verdict")
    args = parser.parse_args()

    verdict = check_clean_release_commit(args.base_ref, args.target_ref)

    if args.json:
        print(json.dumps(verdict, indent=2))
    else:
        status_str = "CLEAN (Evidence-Only)" if verdict["is_clean_release_commit"] else "UNCLEAN (Touches Code)"
        print(f"Release Commit Cleanliness: {status_str}")
        print(f"Range: {verdict['base_ref']}..{verdict['target_ref']}")
        print(f"Allowed files: {verdict['allowed_count']}")
        print(f"Violating files: {verdict['violating_count']}")
        if verdict["violating_files"]:
            print("\nViolating files (code/build modifications):")
            for f in verdict["violating_files"]:
                print(f"  - {f}")

    sys.exit(0 if verdict["is_clean_release_commit"] else 1)

if __name__ == "__main__":
    main()
