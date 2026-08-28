#!/usr/bin/env python3
"""scripts/perf/render_perf_budgets_md.py

Generates tests/perf/reports/PERF_BUDGETS.md from
tests/perf/reports/budget_summary.json (deterministic, byte-identical
output modulo the timestamp).

Bead: bd-perf-budgets-md-generated-nig4e
"""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# Be robust: __file__ is relative when invoked from a different cwd.
SCRIPT = Path(__file__).resolve()
ROOT = SCRIPT.parents[2]  # scripts/perf/ -> scripts/ -> <repo>


def render(budget_summary: dict) -> str:
    out = []
    out.append("# Performance Budgets (auto-generated)\n")
    out.append(f"> Generated: {budget_summary.get('generated_at', 'unknown')}\n")
    out.append(f"> Git commit: {budget_summary.get('git_commit', 'unknown')}\n")
    out.append(f"> Correlation ID: {budget_summary.get('correlation_id', 'unknown')}\n")
    cr = budget_summary.get("claim_readiness", {})
    out.append(f"> Claim readiness: **{cr.get('status', 'unknown')}** "
               f"(performance_claims_authorized={cr.get('performance_claims_authorized', False)})\n")
    if cr.get("status", "") == "blocked":
        out.append("> WARNING: **BLOCKED** - performance claims are NOT authorized in this revision.\n")
    out.append("")

    out.append("## Per-budget results\n")
    out.append("| Budget | Category | CI-enforced | Threshold | Actual | Unit | Status | Source |\n")
    out.append("|---|---|---|---|---|---|---|---|\n")
    for b in budget_summary.get("budget_results", []):
        out.append(
            f"| {b.get('budget_name', '?')} "
            f"| {b.get('category', '?')} "
            f"| {'yes' if b.get('ci_enforced') else 'no'} "
            f"| {b.get('threshold', '?')} "
            f"| {b.get('actual', '?')} "
            f"| {b.get('unit', '?')} "
            f"| **{b.get('status', '?')}** "
            f"| {b.get('source', '?')[:50]} |\n"
        )

    failures = [b for b in budget_summary.get("budget_results", []) if b.get("status") in ("FAIL", "NO_DATA")]
    if failures:
        out.append("\n## Failures and missing data\n")
        for b in failures:
            out.append(f"- **{b.get('budget_name')}**: {b.get('status')} - "
                       f"{b.get('failure_reason', b.get('source', '?'))}\n")

    return "".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--in", dest="inp", type=Path,
                    default=ROOT / "tests/perf/reports/budget_summary.json")
    ap.add_argument("--out", type=Path,
                    default=ROOT / "tests/perf/reports/PERF_BUDGETS.md")
    ap.add_argument("--check", action="store_true",
                    help="Exit non-zero if the file is out of sync with the JSON")
    args = ap.parse_args()

    if not args.inp.exists():
        print(f"FAIL: input not found: {args.inp}", file=sys.stderr)
        return 1
    with open(args.inp) as f:
        bs = json.load(f)
    md = render(bs)
    if args.check and args.out.exists():
        with open(args.out) as f:
            existing = f.read()
        if existing != md:
            print(f"OUT OF SYNC: regenerate {args.out}", file=sys.stderr)
            return 1
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        f.write(md)
    print(f"wrote {args.out} ({len(md)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
