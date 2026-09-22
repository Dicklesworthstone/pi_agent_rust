#!/usr/bin/env python3
"""scripts/perf/render_perf_budgets_md.py

WARNING: DO NOT RUN THIS TO REGENERATE THE CHECKED-IN REPORT. See bd-o9qzt.

Two things write tests/perf/reports/PERF_BUDGETS.md and they disagree. The
file in the repository comes from `tests/perf_budgets.rs`, run as
`PI_GENERATE_PERF_BUDGET_REPORT=1 ... generate_budget_report`; it carries a
summary table, claim readiness with blocking reason codes, per-category
sections, per-budget measurement methodology, failing data contracts with
remediation text, and a CI-enforcement section.

This script renders the same path from the same JSON in a different and
lossier shape -- 189 diff lines against the checked-in file: no methodology,
no data-contract remediations, no run id or strict-mode line, each row's
`source` truncated mid-word at 50 characters, and a "Git commit: unknown"
line because it reads `git_commit` where the artifact's field is
`source_commit`. Running it replaces the good report with this one.

Until somebody decides which generator stays (bd-o9qzt), `--check` is the
only safe mode here: it compares and exits without writing.

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
    if args.check:
        # Previously this compared and then wrote anyway, so `--check` was
        # indistinguishable from a plain run for anything downstream of it and
        # could not be used as a read-only gate. It now returns without
        # touching the file in every path.
        if not args.out.exists():
            print(f"MISSING: {args.out} does not exist", file=sys.stderr)
            return 1
        with open(args.out) as f:
            existing = f.read()
        if existing != md:
            print(f"OUT OF SYNC: {args.out} differs from this renderer's output",
                  file=sys.stderr)
            return 1
        print(f"in sync: {args.out}")
        return 0
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        f.write(md)
    print(f"wrote {args.out} ({len(md)} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
