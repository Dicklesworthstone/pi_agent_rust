#!/usr/bin/env python3
"""Dry-run preflight for RCH artifact sync coverage.

The RCH worker mirror is governed by .rchignore-style rules. This guard checks
that artifact paths needed by remote cargo/test/report gates are not excluded by
those rules before an expensive remote run starts.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


SCHEMA = "pi.rch.artifact_sync_preflight.v1"
POSTCONDITION_ACTION = (
    "Rerun the gate locally or fix RCH artifact retrieval/writeback so the "
    "checked-in evidence artifact updates after the remote command."
)

DEFAULT_REQUIRED_PATHS = (
    "tests/ext_conformance/artifacts",
    "tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json",
    "tests/evidence_bundle/index.json",
    "tests/full_suite_gate/full_suite_verdict.json",
    "tests/perf/reports/bench_schema_registry.json",
)


@dataclass(frozen=True)
class IgnoreRule:
    line: int
    pattern: str
    anchored: bool
    negated: bool

    @property
    def source(self) -> str:
        return ".rchignore"


def normalize_posix_path(path: str) -> str:
    normalized = path.replace("\\", "/").strip()
    while normalized.startswith("./"):
        normalized = normalized[2:]
    return normalized.strip("/")


def load_ignore_rules(ignore_file: Path) -> tuple[list[IgnoreRule], list[str]]:
    errors: list[str] = []
    if not ignore_file.exists():
        return [], [f"ignore file is missing: {ignore_file}"]

    rules: list[IgnoreRule] = []
    try:
        lines = ignore_file.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        return [], [f"failed to read ignore file {ignore_file}: {exc}"]

    for line_number, raw_line in enumerate(lines, start=1):
        stripped = raw_line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        # RCH deliberately treats a leading `!` literally; unlike .gitignore,
        # its source-sync filters do not support negation/re-inclusion.
        negated = False
        stripped = stripped.replace("\\", "/")
        rules.append(
            IgnoreRule(
                line=line_number,
                pattern=stripped,
                anchored=stripped.startswith("/"),
                negated=negated,
            )
        )
    return rules, errors


def core_rule_matches(pattern: str, rel_path: str) -> bool:
    body = pattern.lstrip("/")
    if not body:
        return False

    if body.endswith("/**"):
        base = body[:-3].rstrip("/")
        return rel_path == base or rel_path.startswith(f"{base}/")

    if body.endswith("/"):
        base = body.rstrip("/")
        return rel_path == base or rel_path.startswith(f"{base}/")

    if fnmatch.fnmatchcase(rel_path, body):
        return True

    if "/" not in body:
        return any(fnmatch.fnmatchcase(component, body) for component in rel_path.split("/"))

    return False


def rule_matches(rule: IgnoreRule, rel_path: str) -> bool:
    rel_path = normalize_posix_path(rel_path)
    if rule.anchored:
        return core_rule_matches(rule.pattern, rel_path)

    if core_rule_matches(rule.pattern, rel_path):
        return True

    components = rel_path.split("/")
    for index in range(1, len(components)):
        if core_rule_matches(rule.pattern, "/".join(components[index:])):
            return True
    return False


def resolve_required_path(repo_root: Path, raw_path: str) -> tuple[str, Path]:
    path = Path(raw_path)
    if path.is_absolute():
        full_path = path
        try:
            rel_path = full_path.resolve().relative_to(repo_root.resolve()).as_posix()
        except ValueError:
            rel_path = full_path.absolute().as_posix()
    else:
        rel_path = normalize_posix_path(raw_path)
        full_path = repo_root / rel_path
    return rel_path, full_path


def matched_rule_payload(rule: IgnoreRule, matched: bool) -> dict[str, Any]:
    state = "include" if rule.negated else "exclude"
    return {
        "source": rule.source,
        "line": rule.line,
        "pattern": rule.pattern,
        "anchored": rule.anchored,
        "state": state,
        "matched": matched,
    }


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def artifact_snapshot(repo_root: Path, raw_path: str) -> dict[str, Any]:
    rel_path, full_path = resolve_required_path(repo_root, raw_path)
    snapshot: dict[str, Any] = {
        "path": rel_path,
        "exists": False,
        "kind": "missing",
        "size_bytes": None,
        "mtime_ns": None,
        "sha256": None,
    }
    try:
        stat = full_path.lstat()
    except FileNotFoundError:
        return snapshot
    except OSError as exc:
        snapshot["error"] = str(exc)
        return snapshot

    snapshot["exists"] = True
    if full_path.is_symlink():
        snapshot["kind"] = "symlink"
    elif full_path.is_dir():
        snapshot["kind"] = "directory"
    elif full_path.is_file():
        snapshot["kind"] = "file"
    else:
        snapshot["kind"] = "other"
    snapshot["size_bytes"] = stat.st_size
    snapshot["mtime_ns"] = stat.st_mtime_ns
    if snapshot["kind"] == "file":
        try:
            snapshot["sha256"] = file_sha256(full_path)
        except OSError as exc:
            snapshot["error"] = str(exc)
    return snapshot


def build_postcondition_baseline(repo_root: Path, generated_artifacts: list[str]) -> dict[str, Any]:
    snapshots = []
    for raw_path in generated_artifacts:
        snapshot = artifact_snapshot(repo_root, raw_path)
        snapshots.append({"path": snapshot["path"], "snapshot": snapshot})
    return {
        "schema": SCHEMA,
        "mode": "postcondition-baseline",
        "status": "pass",
        "repo_root": str(repo_root),
        "generated_artifacts": snapshots,
        "violations": [],
        "summary": {
            "generated_artifact_count": len(snapshots),
            "violation_count": 0,
        },
    }


def load_json_file(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def before_snapshots_by_path(
    repo_root: Path, manifest: dict[str, Any]
) -> dict[str, dict[str, Any]]:
    items = manifest.get("generated_artifacts")
    if not isinstance(items, list):
        items = manifest.get("postconditions")
    if not isinstance(items, list):
        return {}

    snapshots: dict[str, dict[str, Any]] = {}
    for item in items:
        if not isinstance(item, dict):
            continue
        snapshot = item.get("snapshot")
        if not isinstance(snapshot, dict):
            snapshot = item.get("before")
        if not isinstance(snapshot, dict):
            snapshot = item
        path = snapshot.get("path") or item.get("path")
        if isinstance(path, str):
            normalized_path, _ = resolve_required_path(repo_root, path)
            snapshots[normalized_path] = snapshot
    return snapshots


def snapshot_changed(before: dict[str, Any], after: dict[str, Any]) -> bool:
    if before.get("exists") != after.get("exists"):
        return True
    if not after.get("exists"):
        return False
    if before.get("kind") != after.get("kind"):
        return True
    return before.get("sha256") != after.get("sha256")


def build_missing_before_manifest_report(repo_root: Path, generated_artifacts: list[str]) -> dict[str, Any]:
    violations = [
        {
            "path": resolve_required_path(repo_root, path)[0],
            "source": "postcondition",
            "line": None,
            "pattern": None,
            "reason": "missing_before_manifest",
            "message": "--before-manifest is required to verify generated artifact writeback",
            "recommended_action": "Run this script before the remote gate with --write-before-manifest, then rerun it after the gate with --before-manifest.",
        }
        for path in generated_artifacts
    ]
    return {
        "schema": SCHEMA,
        "mode": "postcondition",
        "status": "fail",
        "repo_root": str(repo_root),
        "postconditions": [],
        "violations": violations,
        "summary": {
            "generated_artifact_count": len(generated_artifacts),
            "updated_count": 0,
            "unchanged_count": 0,
            "violation_count": len(violations),
        },
    }


def build_postcondition_report(
    repo_root: Path, generated_artifacts: list[str], before_manifest: Path
) -> dict[str, Any]:
    before_manifest_payload = load_json_file(before_manifest)
    manifest_violations: list[dict[str, Any]] = []
    expected_repo_root = str(repo_root.resolve())
    manifest_repo_root = before_manifest_payload.get("repo_root")
    if before_manifest_payload.get("schema") != SCHEMA:
        manifest_violations.append(
            {
                "path": str(before_manifest),
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": "before_manifest_schema_mismatch",
                "message": f"before manifest does not use schema {SCHEMA}",
                "recommended_action": POSTCONDITION_ACTION,
            }
        )
    if before_manifest_payload.get("mode") != "postcondition-baseline":
        manifest_violations.append(
            {
                "path": str(before_manifest),
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": "before_manifest_mode_mismatch",
                "message": "before manifest is not a postcondition baseline",
                "recommended_action": POSTCONDITION_ACTION,
            }
        )
    try:
        resolved_manifest_root = str(Path(str(manifest_repo_root)).resolve())
    except (OSError, TypeError, ValueError):
        resolved_manifest_root = ""
    if resolved_manifest_root != expected_repo_root:
        manifest_violations.append(
            {
                "path": str(before_manifest),
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": "before_manifest_repo_root_mismatch",
                "message": (
                    "before manifest repo_root does not match the current repo root: "
                    f"{manifest_repo_root!r} != {expected_repo_root!r}"
                ),
                "recommended_action": POSTCONDITION_ACTION,
            }
        )
    before_by_path = (
        {}
        if manifest_violations
        else before_snapshots_by_path(repo_root, before_manifest_payload)
    )
    if not generated_artifacts:
        generated_artifacts = list(before_by_path)

    postconditions: list[dict[str, Any]] = []
    violations: list[dict[str, Any]] = list(manifest_violations)
    updated_count = 0
    unchanged_count = 0

    for raw_path in generated_artifacts:
        rel_path, _ = resolve_required_path(repo_root, raw_path)
        before = before_by_path.get(rel_path)
        after = artifact_snapshot(repo_root, raw_path)
        updated = (
            before is not None
            and after.get("kind") == "file"
            and snapshot_changed(before, after)
        )
        if updated:
            updated_count += 1
        else:
            unchanged_count += 1

        item = {
            "path": rel_path,
            "before": before,
            "after": after,
            "updated": updated,
        }
        postconditions.append(item)

        if before is None:
            violations.append(
                {
                    "path": rel_path,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "missing_before_snapshot",
                    "message": f"before manifest has no snapshot for generated artifact: {rel_path}",
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
        elif not after.get("exists"):
            violations.append(
                {
                    "path": rel_path,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "generated_artifact_missing_after_run",
                    "message": f"generated artifact is missing after remote run: {rel_path}",
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
        elif after.get("kind") != "file":
            violations.append(
                {
                    "path": rel_path,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "generated_artifact_not_regular_file",
                    "message": (
                        "generated artifact is not a regular file after remote run: "
                        f"{rel_path} ({after.get('kind', 'unknown')})"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
        elif before.get("error"):
            violations.append(
                {
                    "path": rel_path,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "before_snapshot_error",
                    "message": f"before snapshot failed for generated artifact: {rel_path}",
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
        elif after.get("error"):
            violations.append(
                {
                    "path": rel_path,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "after_snapshot_error",
                    "message": f"after snapshot failed for generated artifact: {rel_path}",
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
        elif not updated:
            violations.append(
                {
                    "path": rel_path,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "generated_artifact_not_updated",
                    "message": (
                        f"generated artifact did not update after remote run: {rel_path}; "
                        "local evidence may still be stale"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )

    return {
        "schema": SCHEMA,
        "mode": "postcondition",
        "status": "fail" if violations else "pass",
        "repo_root": str(repo_root),
        "before_manifest": str(before_manifest),
        "postconditions": postconditions,
        "violations": violations,
        "summary": {
            "generated_artifact_count": len(postconditions),
            "updated_count": updated_count,
            "unchanged_count": unchanged_count,
            "violation_count": len(violations),
        },
    }


def evaluate_required_paths(
    repo_root: Path, rules: list[IgnoreRule], required_paths: list[str]
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    required_results: list[dict[str, Any]] = []
    violations: list[dict[str, Any]] = []

    for raw_path in required_paths:
        rel_path, full_path = resolve_required_path(repo_root, raw_path)
        matched_rules: list[dict[str, Any]] = []
        final_ignored = False
        final_rule: IgnoreRule | None = None

        for rule in rules:
            matched = rule_matches(rule, rel_path)
            if not matched:
                continue
            matched_rules.append(matched_rule_payload(rule, matched=True))
            final_ignored = not rule.negated
            final_rule = rule

        exists = full_path.exists()
        path_result = {
            "path": rel_path,
            "exists": exists,
            "kind": "directory" if full_path.is_dir() else "file" if full_path.is_file() else "missing",
            "matched_rules": matched_rules,
            "included": exists and not final_ignored,
        }
        required_results.append(path_result)

        if not exists:
            violations.append(
                {
                    "path": rel_path,
                    "source": "required_paths",
                    "line": None,
                    "pattern": None,
                    "reason": "missing_required_path",
                    "message": f"required path is missing from the repo: {rel_path}",
                }
            )
            continue

        if final_ignored and final_rule is not None:
            violations.append(
                {
                    "path": rel_path,
                    "source": final_rule.source,
                    "line": final_rule.line,
                    "pattern": final_rule.pattern,
                    "reason": "required_path_excluded",
                    "message": (
                        f"{rel_path} is excluded by {final_rule.source}:{final_rule.line} "
                        f"pattern {final_rule.pattern!r}"
                    ),
                }
            )

    return required_results, violations


def build_report(repo_root: Path, ignore_file: Path, required_paths: list[str]) -> dict[str, Any]:
    rules, load_errors = load_ignore_rules(ignore_file)
    required_results, violations = evaluate_required_paths(repo_root, rules, required_paths)

    for error in load_errors:
        violations.append(
            {
                "path": str(ignore_file),
                "source": ".rchignore",
                "line": None,
                "pattern": None,
                "reason": "ignore_file_error",
                "message": error,
            }
        )

    return {
        "schema": SCHEMA,
        "mode": "dry-run",
        "status": "fail" if violations else "pass",
        "repo_root": str(repo_root),
        "ignore_file": str(ignore_file),
        "required_paths": required_results,
        "violations": violations,
        "summary": {
            "required_path_count": len(required_results),
            "violation_count": len(violations),
        },
    }


def print_text_report(report: dict[str, Any]) -> None:
    if report["mode"] == "postcondition-baseline":
        print("RCH artifact sync postcondition baseline: PASS")
        for item in report["generated_artifacts"]:
            snapshot = item["snapshot"]
            print(f"- {item['path']}: {snapshot['kind']}")
        return

    if report["mode"] == "postcondition":
        print(f"RCH artifact sync postcondition: {report['status'].upper()}")
        for item in report["postconditions"]:
            state = "updated" if item["updated"] else "stale"
            print(f"- {item['path']}: {state}")
        if report["violations"]:
            print("\nViolations:")
            for violation in report["violations"]:
                print(f"- {violation['message']}")
                print(f"  action: {violation['recommended_action']}")
        return

    print(f"RCH artifact sync preflight: {report['status'].upper()}")
    for item in report["required_paths"]:
        state = "included" if item["included"] else "blocked"
        print(f"- {item['path']}: {state} ({item['kind']})")
        for rule in item["matched_rules"]:
            print(
                f"  matched {rule['source']}:{rule['line']} "
                f"{rule['pattern']!r} -> {rule['state']}"
            )

    if report["violations"]:
        print("\nViolations:")
        for violation in report["violations"]:
            print(f"- {violation['message']}")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--mode",
        choices=("preflight", "postcondition"),
        default="preflight",
        help="Run the .rchignore preflight or verify generated artifacts changed after a remote gate.",
    )
    parser.add_argument(
        "--repo-root",
        default=".",
        help="Repository root to evaluate. Defaults to the current directory.",
    )
    parser.add_argument(
        "--ignore-file",
        default=None,
        help="Path to .rchignore. Defaults to <repo-root>/.rchignore.",
    )
    parser.add_argument(
        "--required-path",
        action="append",
        dest="required_paths",
        help="Repo-relative artifact path that must be present in the RCH mirror.",
    )
    parser.add_argument(
        "--generated-artifact",
        action="append",
        dest="generated_artifacts",
        default=[],
        help="Artifact path expected to be generated or rewritten by the remote gate; absolute paths are supported.",
    )
    parser.add_argument(
        "--write-before-manifest",
        type=Path,
        help="Write a pre-run snapshot manifest for --mode postcondition.",
    )
    parser.add_argument(
        "--before-manifest",
        type=Path,
        help="Pre-run snapshot manifest to compare against in --mode postcondition.",
    )
    parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON.")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(args.repo_root).resolve()
    ignore_file = Path(args.ignore_file).resolve() if args.ignore_file else repo_root / ".rchignore"
    required_paths = args.required_paths or list(DEFAULT_REQUIRED_PATHS)

    if args.mode == "postcondition":
        generated_artifacts = list(args.generated_artifacts)
        if args.write_before_manifest is not None:
            if generated_artifacts:
                report = build_postcondition_baseline(repo_root, generated_artifacts)
            else:
                report = build_missing_before_manifest_report(repo_root, generated_artifacts)
                report["violations"] = [
                    {
                        "path": None,
                        "source": "postcondition",
                        "line": None,
                        "pattern": None,
                        "reason": "missing_generated_artifact",
                        "message": "--generated-artifact is required when writing a before manifest",
                        "recommended_action": "Pass at least one --generated-artifact path for the remote gate outputs.",
                    }
                ]
                report["summary"]["violation_count"] = 1
            args.write_before_manifest.parent.mkdir(parents=True, exist_ok=True)
            args.write_before_manifest.write_text(
                json.dumps(report, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
        elif args.before_manifest is None:
            report = build_missing_before_manifest_report(repo_root, generated_artifacts)
        else:
            report = build_postcondition_report(
                repo_root,
                generated_artifacts,
                args.before_manifest.resolve(),
            )
    else:
        report = build_report(repo_root, ignore_file, required_paths)

    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print_text_report(report)
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
