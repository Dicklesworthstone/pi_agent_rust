#!/usr/bin/env python3
"""Build conformance inventory and compatibility validation pack artifacts.

This script now produces two machine-readable artifacts:
1) `inventory.json` — extension + scenario status inventory with failure taxonomy.
2) `compatibility_validation_pack.json` (+ markdown companion) — per-extension
   compatibility breakdown tied to runtime API matrix evidence and reproducible
   local/CI command entrypoints.

Usage:
    python3 tests/ext_conformance/build_inventory.py
    python3 tests/ext_conformance/build_inventory.py --pack-only
    python3 tests/ext_conformance/build_inventory.py --inventory-only
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent.parent
REPORTS_DIR = PROJECT_ROOT / "tests" / "ext_conformance" / "reports"
CONFORMANCE_REPORT = REPORTS_DIR / "conformance" / "conformance_report.json"
SCENARIO_REPORT = REPORTS_DIR / "scenario_conformance.json"
CONFORMANCE_EVENTS = REPORTS_DIR / "conformance" / "conformance_events.jsonl"
RUNTIME_API_MATRIX_REPORT = REPORTS_DIR / "parity" / "runtime_api_matrix.json"
INVENTORY_OUTPUT_PATH = REPORTS_DIR / "inventory.json"
PACK_JSON_OUTPUT_PATH = REPORTS_DIR / "compatibility_validation_pack.json"
PACK_MD_OUTPUT_PATH = REPORTS_DIR / "COMPATIBILITY_VALIDATION_PACK.md"

# ─── Cause taxonomy ────────────────────────────────────────────────────────

CAUSE_TAXONOMY = {
    "manifest_mismatch": {
        "code": "manifest_mismatch",
        "description": "Extension loads but registers different commands/tools/flags than manifest expects",
        "remediation": "Audit manifest or update extension to register expected items",
        "severity": "medium",
    },
    "missing_npm_package": {
        "code": "missing_npm_package",
        "description": "Extension requires an npm package not available as a virtual module stub",
        "remediation": "Add virtual module stub in extensions_js.rs",
        "severity": "medium",
    },
    "multi_file_dependency": {
        "code": "multi_file_dependency",
        "description": "Extension uses relative imports to unbundled sibling/parent modules",
        "remediation": "Bundle multi-file extensions or add relative path resolution",
        "severity": "low",
    },
    "runtime_error": {
        "code": "runtime_error",
        "description": "Extension crashes during initialization (missing data, broken API, FS dependency)",
        "remediation": "Investigate per-extension; may need environment setup or shim fixes",
        "severity": "medium",
    },
    "test_fixture": {
        "code": "test_fixture",
        "description": "Not a real extension; test-only fixture in manifest",
        "remediation": "Exclude from conformance or mark as N/A",
        "severity": "info",
    },
    "mock_gap": {
        "code": "mock_gap",
        "description": "Scenario mock infrastructure doesn't fully support the extension's hostcall pattern",
        "remediation": "Enhance MockSpecInterceptor or ConformanceSession",
        "severity": "high",
    },
    "assertion_gap": {
        "code": "assertion_gap",
        "description": "Scenario expectations not met due to assertion infrastructure limitations",
        "remediation": "Fix assertion logic or update expected values",
        "severity": "high",
    },
    "vcr_stub_gap": {
        "code": "vcr_stub_gap",
        "description": "VCR/stub HTTP mock doesn't produce valid response for extension parser",
        "remediation": "Improve synthetic HTTP response or add extension-specific VCR rules",
        "severity": "medium",
    },
}


def rel_path(path: Path) -> str:
    """Return a project-relative POSIX path."""
    try:
        rel = path.relative_to(PROJECT_ROOT)
    except ValueError:
        rel = path
    return str(rel).replace("\\", "/")


def read_json(path: Path):
    if not path.exists():
        return None
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


def read_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    rows: list[dict] = []
    with open(path, encoding="utf-8") as handle:
        for raw in handle:
            line = raw.strip()
            if not line:
                continue
            rows.append(json.loads(line))
    return rows


def classify_extension_failure(ext_id: str, reason: str) -> str:
    """Classify an extension-level failure into a cause code."""
    if not reason:
        return "runtime_error"

    if ext_id == "base_fixtures":
        return "test_fixture"

    if re.search(r"Missing (command|tool|flag)", reason):
        return "manifest_mismatch"
    if "expects tools but none registered" in reason:
        return "manifest_mismatch"

    if "Unsupported module specifier" in reason:
        spec = re.search(r"specifier: (.+?)(?:\n|$)", reason)
        if spec:
            specifier = spec.group(1).strip()
            if specifier.startswith(".") or specifier.startswith(".."):
                return "multi_file_dependency"
            return "missing_npm_package"
        return "missing_npm_package"

    if "Load error" in reason or "ENOENT" in reason:
        return "runtime_error"
    if "not a function" in reason or "cannot read property" in reason:
        return "runtime_error"
    if "base_fixtures" in reason:
        return "test_fixture"

    return "runtime_error"


def classify_scenario_failure(result: dict) -> str:
    """Classify a scenario-level failure into a cause code."""
    diffs = result.get("diffs", [])
    error = result.get("error", "")

    if error:
        if "No image data" in error or "parse" in error.lower():
            return "vcr_stub_gap"
        return "mock_gap"

    diff_text = " ".join(diffs)
    if "ui_status" in diff_text or "ui_notify" in diff_text:
        return "mock_gap"
    if "exec_called" in diff_text:
        return "mock_gap"
    if "returns_contains" in diff_text or "content_contains" in diff_text:
        return "assertion_gap"

    return "assertion_gap"


def suite_log_rel_path(ext_id: str, suite: str):
    path = REPORTS_DIR / suite / "extensions" / f"{ext_id}.jsonl"
    return rel_path(path) if path.exists() else None


def root_extension_log_rel_path(ext_id: str):
    path = REPORTS_DIR / "extensions" / f"{ext_id}.jsonl"
    return rel_path(path) if path.exists() else None


def build_inventory_data():
    """Build inventory data model from conformance + scenario artifacts."""
    ext_report = read_json(CONFORMANCE_REPORT)
    if ext_report is None:
        print(
            f"ERROR: {CONFORMANCE_REPORT} not found. Run conformance_full_report first.",
            file=sys.stderr,
        )
        sys.exit(1)

    scn_report = read_json(SCENARIO_REPORT)
    if scn_report is None:
        print(
            f"ERROR: {SCENARIO_REPORT} not found. Run scenario_conformance_suite first.",
            file=sys.stderr,
        )
        sys.exit(1)

    ext_entries = []
    ext_failures = {failure["id"]: failure for failure in ext_report.get("failures", [])}

    ext_results = {}
    for entry in read_jsonl(CONFORMANCE_EVENTS):
        ext_results[entry["id"]] = entry

    for ext_id, data in sorted(ext_results.items()):
        status = data["status"]
        cause_code = None
        cause_detail = None

        if status == "fail":
            failure = ext_failures.get(ext_id, {})
            reason = failure.get("reason", data.get("failure_reason", ""))
            cause_code = classify_extension_failure(ext_id, reason)
            cause_detail = reason
        elif status == "skip":
            cause_detail = data.get("failure_reason")

        inv_status = "PASS" if status == "pass" else "N-A" if status == "skip" else "FAIL"

        ext_entries.append(
            {
                "id": ext_id,
                "type": "extension",
                "tier": data.get("tier", 0),
                "status": inv_status,
                "cause_code": cause_code,
                "cause_detail": cause_detail,
                "registrations": {
                    "commands": data.get("commands_registered", 0),
                    "flags": data.get("flags_registered", 0),
                    "tools": data.get("tools_registered", 0),
                    "providers": data.get("providers_registered", 0),
                },
                "duration_ms": data.get("duration_ms", 0),
            }
        )

    scn_entries = []
    for result in scn_report.get("results", []):
        status = result["status"]
        cause_code = None
        cause_detail = None

        if status == "fail":
            cause_code = classify_scenario_failure(result)
            diffs = result.get("diffs", [])
            error = result.get("error")
            cause_detail = error if error else "; ".join(diffs)
        elif status == "skip":
            cause_detail = result.get("skip_reason")
        elif status == "error":
            cause_code = "runtime_error"
            cause_detail = result.get("error")

        inv_status = "PASS" if status == "pass" else "N-A" if status == "skip" else "FAIL"

        scn_entries.append(
            {
                "id": result["scenario_id"],
                "type": "scenario",
                "extension_id": result["extension_id"],
                "kind": result["kind"],
                "summary": result["summary"],
                "status": inv_status,
                "source_tier": result.get("source_tier", ""),
                "runtime_tier": result.get("runtime_tier", ""),
                "cause_code": cause_code,
                "cause_detail": cause_detail,
                "duration_ms": result.get("duration_ms", 0),
            }
        )

    ext_pass = sum(1 for item in ext_entries if item["status"] == "PASS")
    ext_fail = sum(1 for item in ext_entries if item["status"] == "FAIL")
    ext_na = sum(1 for item in ext_entries if item["status"] == "N-A")

    scn_pass = sum(1 for item in scn_entries if item["status"] == "PASS")
    scn_fail = sum(1 for item in scn_entries if item["status"] == "FAIL")
    scn_na = sum(1 for item in scn_entries if item["status"] == "N-A")

    cause_counts = {}
    for entry in ext_entries + scn_entries:
        code = entry.get("cause_code")
        if code:
            cause_counts[code] = cause_counts.get(code, 0) + 1

    inventory = {
        "schema": "pi.ext.inventory.v1",
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "summary": {
            "extensions": {
                "total": len(ext_entries),
                "pass": ext_pass,
                "fail": ext_fail,
                "na": ext_na,
                "pass_rate_pct": round(ext_pass / max(ext_pass + ext_fail, 1) * 100, 1),
            },
            "scenarios": {
                "total": len(scn_entries),
                "pass": scn_pass,
                "fail": scn_fail,
                "na": scn_na,
                "pass_rate_pct": round(scn_pass / max(scn_pass + scn_fail, 1) * 100, 1),
            },
        },
        "cause_taxonomy": {
            code: {**meta, "count": cause_counts.get(code, 0)}
            for code, meta in CAUSE_TAXONOMY.items()
        },
        "cause_distribution": dict(sorted(cause_counts.items(), key=lambda item: -item[1])),
        "extensions": ext_entries,
        "scenarios": scn_entries,
        "regression_thresholds": {
            "tier1_pass_rate_min_pct": 100.0,
            "tier2_pass_rate_min_pct": 95.0,
            "overall_pass_rate_min_pct": 80.0,
            "scenario_pass_rate_min_pct": 85.0,
            "max_new_failures": 3,
        },
    }

    return inventory, ext_report


def build_validation_pack(inventory: dict, ext_report: dict) -> dict:
    """Build compatibility validation pack tied to runtime API matrix evidence."""
    runtime_matrix = read_json(RUNTIME_API_MATRIX_REPORT) or {}
    runtime_summary = runtime_matrix.get("summary", {})
    runtime_failures = []

    for entry in runtime_matrix.get("entries", []):
        if entry.get("status") == "pass":
            continue
        runtime_failures.append(
            {
                "surface": entry.get("surface"),
                "module": entry.get("module"),
                "api": entry.get("api"),
                "status": entry.get("status"),
                "call_count": entry.get("call_count", 0),
                "diagnostics": entry.get("diagnostics"),
                "evidence_file": entry.get("evidence_file"),
            }
        )

    runtime_failures.sort(
        key=lambda item: (
            -int(item.get("call_count") or 0),
            str(item.get("module") or ""),
            str(item.get("api") or ""),
        )
    )

    scenario_by_extension: dict[str, dict[str, int]] = {}
    for scenario in inventory.get("scenarios", []):
        ext_id = scenario.get("extension_id")
        if not ext_id:
            continue
        slot = scenario_by_extension.setdefault(ext_id, {"PASS": 0, "FAIL": 0, "N-A": 0})
        status = scenario.get("status")
        if status in slot:
            slot[status] += 1

    extension_rows = []
    for ext in inventory.get("extensions", []):
        ext_id = ext["id"]
        scenario = scenario_by_extension.get(ext_id, {"PASS": 0, "FAIL": 0, "N-A": 0})
        extension_rows.append(
            {
                "id": ext_id,
                "status": ext["status"],
                "tier": ext.get("tier"),
                "cause_code": ext.get("cause_code"),
                "cause_detail": ext.get("cause_detail"),
                "duration_ms": ext.get("duration_ms", 0),
                "registrations": ext.get("registrations", {}),
                "scenario_summary": {
                    "pass": scenario.get("PASS", 0),
                    "fail": scenario.get("FAIL", 0),
                    "na": scenario.get("N-A", 0),
                },
                "logs": {
                    "conformance_extension_log": root_extension_log_rel_path(ext_id),
                    "smoke_extension_log": suite_log_rel_path(ext_id, "smoke"),
                    "parity_extension_log": suite_log_rel_path(ext_id, "parity"),
                    "conformance_events_jsonl": rel_path(CONFORMANCE_EVENTS),
                    "scenario_report_json": rel_path(SCENARIO_REPORT),
                },
                "runtime_api_matrix_reference": rel_path(RUNTIME_API_MATRIX_REPORT),
            }
        )

    extension_rows.sort(key=lambda row: (row["status"] != "FAIL", row["id"]))

    conformance_counts = inventory.get("summary", {}).get("extensions", {})
    scenario_counts = inventory.get("summary", {}).get("scenarios", {})

    commands_local = [
        "cargo test --test ext_conformance_generated --features ext-conformance conformance_full_report -- --nocapture",
        "cargo test --test ext_conformance_matrix generate_runtime_api_matrix_report -- --nocapture",
        "cargo test --test ext_conformance_scenarios --features ext-conformance scenario_conformance_suite -- --nocapture",
        "python3 tests/ext_conformance/build_inventory.py",
    ]
    commands_ci = [
        "./scripts/e2e/run_all.sh --profile quick --skip-lint --skip-unit --suite e2e_tools",
        "./scripts/e2e/run_all.sh --profile quick --skip-lint --skip-unit --suite e2e_extension_registration",
    ]

    pack = {
        "schema": "pi.ext.compatibility_validation_pack.v1",
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "summary": {
            "extensions": conformance_counts,
            "scenarios": scenario_counts,
            "runtime_api_matrix": runtime_summary,
            "conformance_report": {
                "manifest_count": ext_report.get("manifest_count", 0),
                "tested": ext_report.get("tested", 0),
                "passed": ext_report.get("passed", 0),
                "failed": ext_report.get("failed", 0),
                "skipped": ext_report.get("skipped", 0),
                "pass_rate_pct": ext_report.get("pass_rate_pct", 0.0),
            },
        },
        "commands": {
            "local": commands_local,
            "ci": commands_ci,
        },
        "source_reports": {
            "conformance_report_json": rel_path(CONFORMANCE_REPORT),
            "conformance_events_jsonl": rel_path(CONFORMANCE_EVENTS),
            "scenario_report_json": rel_path(SCENARIO_REPORT),
            "runtime_api_matrix_json": rel_path(RUNTIME_API_MATRIX_REPORT),
            "inventory_json": rel_path(INVENTORY_OUTPUT_PATH),
        },
        "runtime_api_matrix": {
            "summary": runtime_summary,
            "failing_entries": runtime_failures,
        },
        "cause_distribution": inventory.get("cause_distribution", {}),
        "per_extension_breakdown": extension_rows,
    }
    return pack


def render_validation_pack_markdown(pack: dict) -> str:
    """Render a human-readable compatibility validation pack summary."""
    ext_summary = pack.get("summary", {}).get("extensions", {})
    scenario_summary = pack.get("summary", {}).get("scenarios", {})
    runtime_summary = pack.get("summary", {}).get("runtime_api_matrix", {})
    runtime_failures = pack.get("runtime_api_matrix", {}).get("failing_entries", [])
    extensions = pack.get("per_extension_breakdown", [])

    lines: list[str] = []
    lines.append("# Compatibility Validation Pack")
    lines.append("")
    lines.append(f"> Generated: {pack.get('generated_at')}")
    lines.append("")
    lines.append("## Summary")
    lines.append("")
    lines.append("| Signal | Value |")
    lines.append("|--------|-------|")
    lines.append(
        f"| Extension PASS/FAIL/N-A | {ext_summary.get('pass', 0)}/{ext_summary.get('fail', 0)}/{ext_summary.get('na', 0)} |"
    )
    lines.append(
        f"| Extension pass rate | {ext_summary.get('pass_rate_pct', 0.0)}% |"
    )
    lines.append(
        f"| Scenario PASS/FAIL/N-A | {scenario_summary.get('pass', 0)}/{scenario_summary.get('fail', 0)}/{scenario_summary.get('na', 0)} |"
    )
    lines.append(
        f"| Scenario pass rate | {scenario_summary.get('pass_rate_pct', 0.0)}% |"
    )
    lines.append(
        f"| Runtime API matrix PASS/FAIL | {runtime_summary.get('pass', 0)}/{runtime_summary.get('fail', 0)} |"
    )
    lines.append("")
    lines.append("## Reproducible Commands")
    lines.append("")
    lines.append("```bash")
    for command in pack.get("commands", {}).get("local", []):
        lines.append(command)
    for command in pack.get("commands", {}).get("ci", []):
        lines.append(command)
    lines.append("```")
    lines.append("")

    if runtime_failures:
        lines.append("## Runtime API Matrix Gaps")
        lines.append("")
        lines.append("| Surface | Module | API | Calls | Status |")
        lines.append("|---------|--------|-----|-------|--------|")
        for item in runtime_failures[:25]:
            lines.append(
                "| {surface} | `{module}` | `{api}` | {calls} | {status} |".format(
                    surface=item.get("surface", ""),
                    module=item.get("module", ""),
                    api=item.get("api", ""),
                    calls=item.get("call_count", 0),
                    status=item.get("status", ""),
                )
            )
        lines.append("")

    lines.append("## Per-Extension PASS/FAIL Breakdown")
    lines.append("")
    lines.append(
        "| Extension | Status | Tier | Scenarios (P/F/N-A) | Cause | Conformance log | Smoke log | Parity log |"
    )
    lines.append(
        "|-----------|--------|------|---------------------|-------|-----------------|-----------|------------|"
    )
    for row in extensions:
        scenario = row.get("scenario_summary", {})
        logs = row.get("logs", {})
        cause = (row.get("cause_code") or "").replace("|", "/")
        lines.append(
            "| `{ext}` | {status} | {tier} | {p}/{f}/{na} | {cause} | `{conformance}` | `{smoke}` | `{parity}` |".format(
                ext=row.get("id", ""),
                status=row.get("status", ""),
                tier=row.get("tier", ""),
                p=scenario.get("pass", 0),
                f=scenario.get("fail", 0),
                na=scenario.get("na", 0),
                cause=cause,
                conformance=logs.get("conformance_extension_log") or "-",
                smoke=logs.get("smoke_extension_log") or "-",
                parity=logs.get("parity_extension_log") or "-",
            )
        )

    lines.append("")
    return "\n".join(lines) + "\n"


def write_json(path: Path, payload: dict):
    with open(path, "w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2, sort_keys=False)
        handle.write("\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--pack-only",
        action="store_true",
        help="Only write compatibility validation pack outputs (skip inventory.json).",
    )
    parser.add_argument(
        "--inventory-only",
        action="store_true",
        help="Only write inventory.json (skip validation pack outputs).",
    )
    args = parser.parse_args()
    if args.pack_only and args.inventory_only:
        parser.error("cannot use --pack-only and --inventory-only together")
    return args


def main():
    args = parse_args()
    inventory, ext_report = build_inventory_data()
    os.makedirs(REPORTS_DIR, exist_ok=True)

    wrote_inventory = False
    wrote_pack = False

    if not args.pack_only:
        write_json(INVENTORY_OUTPUT_PATH, inventory)
        wrote_inventory = True

    if not args.inventory_only:
        pack = build_validation_pack(inventory, ext_report)
        write_json(PACK_JSON_OUTPUT_PATH, pack)
        with open(PACK_MD_OUTPUT_PATH, "w", encoding="utf-8") as handle:
            handle.write(render_validation_pack_markdown(pack))
        wrote_pack = True

    if wrote_inventory:
        ext = inventory["summary"]["extensions"]
        scn = inventory["summary"]["scenarios"]
        print(f"Inventory written to {INVENTORY_OUTPUT_PATH}")
        print(f"  Extensions: {ext['pass']}/{ext['total']} PASS ({ext['pass_rate_pct']}%)")
        print(f"  Scenarios:  {scn['pass']}/{scn['total']} PASS ({scn['pass_rate_pct']}%)")
        print(f"  Cause distribution: {json.dumps(inventory['cause_distribution'])}")

    if wrote_pack:
        print(f"Validation pack written to {PACK_JSON_OUTPUT_PATH}")
        print(f"Validation pack markdown written to {PACK_MD_OUTPUT_PATH}")
        print(
            f"  Runtime API matrix source: {rel_path(RUNTIME_API_MATRIX_REPORT)}"
        )


if __name__ == "__main__":
    main()
