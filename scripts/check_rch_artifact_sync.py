#!/usr/bin/env python3
"""Dry-run preflight for RCH artifact sync coverage.

The RCH worker mirror is governed by project transfer config, mandatory runtime
excludes, and project-local ``.rchignore`` rules. This guard checks that artifact
paths needed by remote cargo/test/report gates are not excluded by that effective
policy before an expensive remote run starts.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import stat as stat_mode
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


SCHEMA = "pi.rch.artifact_sync_preflight.v1"
POSTCONDITION_ACTION = (
    "Rerun the gate locally or fix RCH artifact retrieval/writeback so the "
    "checked-in evidence artifact updates after the remote command."
)

# Keep these aligned with RCH 1.0.58's transfer::CONFIG_EXCLUDE_REWRITES,
# REMOTE_RUNTIME_EXCLUDE_PATTERNS, and SOURCE_EPHEMERAL_EXCLUDE_PATTERNS. The
# project config replaces lower config arrays, but these runtime protections are
# appended unconditionally by RCH.
CONFIG_EXCLUDE_REWRITES = {
    "core.*": "core.[0-9]*",
    ".core.*": ".core.[0-9]*",
    ".git/objects/": ".git/",
}
MANDATORY_RCH_EXCLUDES = (
    ".rch-target/",
    ".rch-target-*/",
    ".rch-tmp/",
    ".rch-go/",
    ".franken_whisper/tools/ffmpeg/",
    ".venv/",
    ".venv-*/",
    "venv/",
    "venv-*/",
    "__pycache__/",
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
    source: str
    line: int | None
    pattern: str
    anchored: bool
    negated: bool


def normalize_posix_path(path: str) -> str:
    normalized = path.replace("\\", "/").strip()
    while normalized.startswith("./"):
        normalized = normalized[2:]
    return normalized.strip("/")


def normalize_config_exclude_pattern(pattern: str) -> str:
    return CONFIG_EXCLUDE_REWRITES.get(pattern, pattern)


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
                source=".rchignore",
                line=line_number,
                pattern=stripped,
                anchored=stripped.startswith("/"),
                negated=negated,
            )
        )
    return rules, errors


def load_config_rules(config_file: Path) -> tuple[list[IgnoreRule], list[str]]:
    """Load the project-level transfer excludes that replace lower config layers."""
    if not config_file.exists():
        return [], []

    try:
        content = config_file.read_text(encoding="utf-8")
        payload = tomllib.loads(content)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        return [], [f"failed to read RCH config {config_file}: {exc}"]

    transfer = payload.get("transfer")
    if transfer is None:
        return [], []
    if not isinstance(transfer, dict):
        return [], [f"RCH config transfer section is not a table: {config_file}"]
    patterns = transfer.get("exclude_patterns")
    if patterns is None:
        return [], []
    if not isinstance(patterns, list):
        return [], [f"RCH config transfer.exclude_patterns is not an array: {config_file}"]

    lines = content.splitlines()
    rules: list[IgnoreRule] = []
    errors: list[str] = []
    for index, raw_pattern in enumerate(patterns):
        if not isinstance(raw_pattern, str):
            errors.append(
                "RCH config transfer.exclude_patterns contains a non-string "
                f"entry at index {index}: {config_file}"
            )
            continue
        raw_pattern_text = raw_pattern.strip().replace("\\", "/")
        if not raw_pattern_text:
            errors.append(
                "RCH config transfer.exclude_patterns contains an empty "
                f"entry at index {index}: {config_file}"
            )
            continue
        line_number = next(
            (
                line_index
                for line_index, line in enumerate(lines, start=1)
                if raw_pattern_text in line
            ),
            None,
        )
        pattern = normalize_config_exclude_pattern(raw_pattern_text)
        rules.append(
            IgnoreRule(
                source=".rch/config.toml",
                line=line_number,
                pattern=pattern,
                anchored=pattern.startswith("/"),
                negated=False,
            )
        )
    return rules, errors


def mandatory_rch_rules() -> list[IgnoreRule]:
    return [
        IgnoreRule(
            source="RCH mandatory exclusions",
            line=None,
            pattern=pattern,
            anchored=False,
            negated=False,
        )
        for pattern in MANDATORY_RCH_EXCLUDES
    ]


def component_glob_matches(
    pattern_components: tuple[str, ...],
    path_components: tuple[str, ...],
    *,
    allow_descendants: bool,
) -> bool:
    """Match rsync-style path components without allowing ``*`` across ``/``."""
    if not pattern_components:
        return allow_descendants or not path_components

    pattern_head = pattern_components[0]
    pattern_tail = pattern_components[1:]
    if pattern_head == "**":
        return component_glob_matches(
            pattern_tail,
            path_components,
            allow_descendants=allow_descendants,
        ) or bool(path_components) and component_glob_matches(
            pattern_components,
            path_components[1:],
            allow_descendants=allow_descendants,
        )

    return bool(path_components) and fnmatch.fnmatchcase(path_components[0], pattern_head) and (
        component_glob_matches(
            pattern_tail,
            path_components[1:],
            allow_descendants=allow_descendants,
        )
    )


def core_rule_matches(pattern: str, rel_path: str) -> bool:
    body = pattern.lstrip("/")
    if not body:
        return False

    directory_rule = body.endswith("/")
    body = body.rstrip("/")
    if not body:
        return False

    pattern_components = tuple(component for component in body.split("/") if component)
    path_components = tuple(component for component in rel_path.split("/") if component)
    return component_glob_matches(
        pattern_components,
        path_components,
        allow_descendants=directory_rule,
    )


def rule_matches(rule: IgnoreRule, rel_path: str) -> bool:
    rel_path = normalize_posix_path(rel_path)
    if rule.anchored:
        return core_rule_matches(rule.pattern, rel_path)

    body = rule.pattern.strip("/")
    if "/" not in body:
        # An rsync exclude without a slash matches a basename at any depth. If
        # that basename is a directory, excluding it also excludes every path
        # below it, so inspecting every component is the correct conservative
        # answer for a required descendant.
        return any(
            fnmatch.fnmatchcase(component, body)
            for component in rel_path.split("/")
        )

    if core_rule_matches(rule.pattern, rel_path):
        return True

    components = rel_path.split("/")
    for index in range(1, len(components)):
        if core_rule_matches(rule.pattern, "/".join(components[index:])):
            return True
    return False


def resolve_required_path(repo_root: Path, raw_path: str) -> tuple[str, Path]:
    path = Path(raw_path)
    repo_root = Path(os.path.abspath(os.path.normpath(str(repo_root))))
    if path.is_absolute():
        # Keep identity lexical and stable across the before/after interval.
        # Path.resolve() follows symlinks, so re-resolving a manifest path after
        # a parent was replaced by a symlink can silently change its identity.
        full_path = Path(os.path.abspath(os.path.normpath(str(path))))
        try:
            rel_path = full_path.relative_to(repo_root).as_posix()
        except ValueError:
            rel_path = full_path.as_posix()
    else:
        rel_path = normalize_posix_path(raw_path)
        full_path = Path(os.path.abspath(os.path.normpath(str(repo_root / rel_path))))
    return rel_path, full_path


def relative_path_escapes_repo(raw_path: str) -> bool:
    path = Path(raw_path)
    if path.is_absolute():
        return False
    rel_path = normalize_posix_path(raw_path)
    return (
        not rel_path
        or rel_path == ".."
        or rel_path.startswith("../")
        or "/../" in rel_path
    )


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


def first_symlink_ancestor(path: Path) -> Path | None:
    absolute = path.absolute()
    current = Path(absolute.anchor)
    for component in absolute.parts[1:-1]:
        current /= component
        try:
            metadata = current.lstat()
        except FileNotFoundError:
            return None
        if stat_mode.S_ISLNK(metadata.st_mode):
            return current
    return None


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
    if relative_path_escapes_repo(raw_path):
        snapshot["kind"] = "invalid"
        snapshot["error"] = (
            "generated artifact must be repo-relative without parent traversal "
            "or an absolute path"
        )
        return snapshot
    try:
        symlink_ancestor = first_symlink_ancestor(full_path)
    except OSError as exc:
        snapshot["error"] = f"failed to inspect generated artifact ancestors: {exc}"
        return snapshot
    if symlink_ancestor is not None:
        snapshot["exists"] = True
        snapshot["kind"] = "symlink_ancestor"
        snapshot["symlink_ancestor"] = str(symlink_ancestor)
        return snapshot
    try:
        metadata = full_path.lstat()
    except FileNotFoundError:
        return snapshot
    except OSError as exc:
        snapshot["error"] = str(exc)
        return snapshot

    snapshot["exists"] = True
    if stat_mode.S_ISLNK(metadata.st_mode):
        snapshot["kind"] = "symlink"
    elif stat_mode.S_ISDIR(metadata.st_mode):
        snapshot["kind"] = "directory"
    elif stat_mode.S_ISREG(metadata.st_mode):
        snapshot["kind"] = "file"
    else:
        snapshot["kind"] = "other"
    snapshot["size_bytes"] = metadata.st_size
    snapshot["mtime_ns"] = metadata.st_mtime_ns
    if snapshot["kind"] == "file":
        try:
            snapshot["sha256"] = file_sha256(full_path)
        except OSError as exc:
            snapshot["error"] = str(exc)
    return snapshot


def build_postcondition_baseline(
    repo_root: Path,
    generated_artifacts: list[str],
    invocation_identity: dict[str, str] | None = None,
) -> dict[str, Any]:
    snapshots = []
    violations: list[dict[str, Any]] = []
    seen_paths: set[str] = set()
    for raw_path in generated_artifacts:
        snapshot = artifact_snapshot(repo_root, raw_path)
        snapshots.append({"path": snapshot["path"], "snapshot": snapshot})
        if snapshot["path"] in seen_paths:
            violations.append(
                {
                    "path": snapshot["path"],
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "duplicate_generated_artifact_request",
                    "message": (
                        "postcondition baseline request contains duplicate generated artifact: "
                        f"{snapshot['path']}"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
        else:
            seen_paths.add(snapshot["path"])
        if snapshot.get("error"):
            violations.append(
                {
                    "path": snapshot["path"],
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "before_snapshot_error",
                    "message": (
                        "failed to capture the generated artifact baseline: "
                        f"{snapshot['path']}"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
        elif snapshot.get("kind") not in {"missing", "file"}:
            violations.append(
                {
                    "path": snapshot["path"],
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "before_snapshot_not_regular_file",
                    "message": (
                        "generated artifact baseline is neither missing nor a regular file: "
                        f"{snapshot['path']} ({snapshot.get('kind', 'unknown')})"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
    return {
        "schema": SCHEMA,
        "mode": "postcondition-baseline",
        "status": "fail" if violations else "pass",
        "repo_root": str(repo_root),
        "invocation_identity": invocation_identity or {},
        "generated_artifacts": snapshots,
        "violations": violations,
        "summary": {
            "generated_artifact_count": len(snapshots),
            "violation_count": len(violations),
        },
    }


def load_json_file(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def build_before_manifest_error_report(
    repo_root: Path,
    generated_artifacts: list[str],
    before_manifest: Path,
    reason: str,
    message: str,
) -> dict[str, Any]:
    return {
        "schema": SCHEMA,
        "mode": "postcondition",
        "status": "fail",
        "repo_root": str(repo_root),
        "before_manifest": str(before_manifest),
        "postconditions": [],
        "violations": [
            {
                "path": str(before_manifest),
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": reason,
                "message": message,
                "recommended_action": POSTCONDITION_ACTION,
            }
        ],
        "summary": {
            "generated_artifact_count": len(generated_artifacts),
            "updated_count": 0,
            "unchanged_count": len(generated_artifacts),
            "violation_count": 1,
        },
    }


def append_before_manifest_write_error(
    report: dict[str, Any], before_manifest: Path, error: OSError
) -> None:
    report["status"] = "fail"
    report["violations"].append(
        {
            "path": str(before_manifest),
            "source": "postcondition",
            "line": None,
            "pattern": None,
            "reason": "before_manifest_write_error",
            "message": f"failed to write before manifest {before_manifest}: {error}",
            "recommended_action": POSTCONDITION_ACTION,
        }
    )
    report["summary"]["violation_count"] = len(report["violations"])


def validate_invocation_identity(
    raw_identity: Any,
    expected_identity: dict[str, str] | None,
    before_manifest: Path,
) -> list[dict[str, Any]]:
    violations: list[dict[str, Any]] = []
    valid_identity = isinstance(raw_identity, dict) and all(
        key in {"source_commit", "correlation_id", "command_digest"}
        and isinstance(value, str)
        and bool(value)
        for key, value in raw_identity.items()
    )
    if not valid_identity:
        violations.append(
            {
                "path": str(before_manifest),
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": "before_manifest_identity_invalid",
                "message": "before manifest invocation_identity is not a valid string map",
                "recommended_action": POSTCONDITION_ACTION,
            }
        )
        return violations

    if expected_identity is not None and raw_identity != expected_identity:
        violations.append(
            {
                "path": str(before_manifest),
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": "before_manifest_identity_mismatch",
                "message": (
                    "before manifest invocation identity does not match this postcondition; "
                    f"manifest={raw_identity!r}, expected={expected_identity!r}"
                ),
                "recommended_action": POSTCONDITION_ACTION,
            }
        )
    elif expected_identity is None and raw_identity:
        violations.append(
            {
                "path": str(before_manifest),
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": "before_manifest_identity_not_asserted",
                "message": (
                    "before manifest is invocation-bound, but the postcondition did not "
                    "provide the same identity fields"
                ),
                "recommended_action": POSTCONDITION_ACTION,
            }
        )
    return violations


def before_snapshots_by_path(
    repo_root: Path, manifest: dict[str, Any]
) -> tuple[dict[str, dict[str, Any]], list[dict[str, Any]]]:
    items = manifest.get("generated_artifacts")
    if not isinstance(items, list):
        items = manifest.get("postconditions")
    if not isinstance(items, list):
        return {}, [
            {
                "path": None,
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": "before_manifest_artifact_set_invalid",
                "message": "before manifest does not contain a generated-artifact array",
                "recommended_action": POSTCONDITION_ACTION,
            }
        ]

    snapshots: dict[str, dict[str, Any]] = {}
    violations: list[dict[str, Any]] = []
    for index, item in enumerate(items):
        if not isinstance(item, dict):
            violations.append(
                {
                    "path": None,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "before_manifest_snapshot_invalid",
                    "message": f"before manifest artifact entry {index} is not an object",
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
            continue
        snapshot = item.get("snapshot")
        if not isinstance(snapshot, dict):
            snapshot = item.get("before")
        if not isinstance(snapshot, dict):
            snapshot = item
        snapshot_path = snapshot.get("path")
        item_path = item.get("path")
        path = snapshot_path or item_path
        if not isinstance(path, str) or not path.strip():
            violations.append(
                {
                    "path": None,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "before_manifest_snapshot_invalid",
                    "message": f"before manifest artifact entry {index} has no valid path",
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
            continue
        normalized_path, _ = resolve_required_path(repo_root, path)
        if isinstance(snapshot_path, str) and isinstance(item_path, str):
            normalized_item_path, _ = resolve_required_path(repo_root, item_path)
            normalized_snapshot_path, _ = resolve_required_path(repo_root, snapshot_path)
            if normalized_item_path != normalized_snapshot_path:
                violations.append(
                    {
                        "path": normalized_path,
                        "source": "postcondition",
                        "line": None,
                        "pattern": None,
                        "reason": "before_manifest_snapshot_path_mismatch",
                        "message": (
                            "before manifest artifact entry and snapshot disagree on path: "
                            f"{normalized_item_path} != {normalized_snapshot_path}"
                        ),
                        "recommended_action": POSTCONDITION_ACTION,
                    }
                )
                continue
        exists = snapshot.get("exists")
        kind = snapshot.get("kind")
        sha256 = snapshot.get("sha256")
        size_bytes = snapshot.get("size_bytes")
        mtime_ns = snapshot.get("mtime_ns")
        valid_file_snapshot = (
            exists is True
            and kind == "file"
            and isinstance(sha256, str)
            and len(sha256) == 64
            and all(character in "0123456789abcdef" for character in sha256)
            and type(size_bytes) is int
            and size_bytes >= 0
            and type(mtime_ns) is int
            and mtime_ns >= 0
        )
        valid_missing_snapshot = (
            exists is False
            and kind == "missing"
            and sha256 is None
            and size_bytes is None
            and mtime_ns is None
        )
        if snapshot.get("error") or not (valid_file_snapshot or valid_missing_snapshot):
            violations.append(
                {
                    "path": normalized_path,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "before_manifest_snapshot_invalid",
                    "message": (
                        "before manifest contains a malformed generated-artifact snapshot: "
                        f"{normalized_path}"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
            continue
        if normalized_path in snapshots:
            violations.append(
                {
                    "path": normalized_path,
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "before_manifest_duplicate_artifact",
                    "message": (
                        "before manifest contains duplicate generated-artifact identity: "
                        f"{normalized_path}"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
            continue
        snapshots[normalized_path] = snapshot
    return snapshots, violations


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
    repo_root: Path,
    generated_artifacts: list[str],
    before_manifest: Path,
    invocation_identity: dict[str, str] | None = None,
) -> dict[str, Any]:
    try:
        before_manifest_payload = load_json_file(before_manifest)
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        return build_before_manifest_error_report(
            repo_root,
            generated_artifacts,
            before_manifest,
            "before_manifest_read_error",
            f"failed to read before manifest {before_manifest}: {exc}",
        )
    if not isinstance(before_manifest_payload, dict):
        return build_before_manifest_error_report(
            repo_root,
            generated_artifacts,
            before_manifest,
            "before_manifest_not_object",
            f"before manifest must contain a JSON object: {before_manifest}",
        )
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
    if before_manifest_payload.get("status") != "pass":
        manifest_violations.append(
            {
                "path": str(before_manifest),
                "source": "postcondition",
                "line": None,
                "pattern": None,
                "reason": "before_manifest_status_mismatch",
                "message": "before manifest did not record a successful baseline capture",
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
    manifest_violations.extend(
        validate_invocation_identity(
            before_manifest_payload.get("invocation_identity", {}),
            invocation_identity,
            before_manifest,
        )
    )
    before_by_path, artifact_set_violations = before_snapshots_by_path(
        repo_root, before_manifest_payload
    )
    manifest_violations.extend(artifact_set_violations)
    if not generated_artifacts:
        generated_artifacts = list(before_by_path)
    else:
        requested_paths = [
            resolve_required_path(repo_root, raw_path)[0]
            for raw_path in generated_artifacts
        ]
        duplicate_requested_paths = sorted(
            {
                path
                for path in requested_paths
                if requested_paths.count(path) > 1
            }
        )
        if duplicate_requested_paths:
            manifest_violations.append(
                {
                    "path": str(before_manifest),
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "duplicate_generated_artifact_request",
                    "message": (
                        "postcondition request contains duplicate generated artifacts: "
                        f"{duplicate_requested_paths}"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )
        before_paths = set(before_by_path)
        requested_path_set = set(requested_paths)
        if requested_path_set != before_paths:
            manifest_violations.append(
                {
                    "path": str(before_manifest),
                    "source": "postcondition",
                    "line": None,
                    "pattern": None,
                    "reason": "before_manifest_artifact_set_mismatch",
                    "message": (
                        "postcondition artifact set does not exactly match the before manifest; "
                        f"missing_from_request={sorted(before_paths - requested_path_set)}, "
                        f"missing_from_manifest={sorted(requested_path_set - before_paths)}"
                    ),
                    "recommended_action": POSTCONDITION_ACTION,
                }
            )

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
            and not before.get("error")
            and not after.get("error")
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

        if (
            Path(raw_path).is_absolute()
            or not rel_path
            or rel_path == ".."
            or rel_path.startswith("../")
            or "/../" in rel_path
        ):
            required_results.append(
                {
                    "path": rel_path,
                    "exists": False,
                    "kind": "invalid",
                    "matched_rules": [],
                    "included": False,
                }
            )
            violations.append(
                {
                    "path": rel_path,
                    "source": "required_paths",
                    "line": None,
                    "pattern": None,
                    "reason": "invalid_required_path",
                    "message": (
                        "required artifact path must be a normalized repo-relative path: "
                        f"{raw_path}"
                    ),
                }
            )
            continue

        for rule in rules:
            matched = rule_matches(rule, rel_path)
            if not matched:
                continue
            matched_rules.append(matched_rule_payload(rule, matched=True))
            final_ignored = not rule.negated
            final_rule = rule
            # RCH passes excludes to rsync in effective-config order. Rsync's
            # filter evaluation is first-match-wins, and RCH has no include
            # rules that could reverse an earlier exclusion.
            break

        try:
            metadata = full_path.lstat()
        except FileNotFoundError:
            metadata = None
        except OSError as exc:
            violations.append(
                {
                    "path": rel_path,
                    "source": "required_paths",
                    "line": None,
                    "pattern": None,
                    "reason": "required_path_inspection_error",
                    "message": f"failed to inspect required path {rel_path}: {exc}",
                }
            )
            required_results.append(
                {
                    "path": rel_path,
                    "exists": False,
                    "kind": "inspection_error",
                    "matched_rules": matched_rules,
                    "included": False,
                }
            )
            continue
        exists = metadata is not None
        try:
            symlink_ancestor = first_symlink_ancestor(full_path)
        except OSError as exc:
            violations.append(
                {
                    "path": rel_path,
                    "source": "required_paths",
                    "line": None,
                    "pattern": None,
                    "reason": "required_path_inspection_error",
                    "message": f"failed to inspect required path ancestors {rel_path}: {exc}",
                }
            )
            required_results.append(
                {
                    "path": rel_path,
                    "exists": False,
                    "kind": "inspection_error",
                    "matched_rules": matched_rules,
                    "included": False,
                }
            )
            continue
        if symlink_ancestor is not None:
            exists = True
            kind = "symlink_ancestor"
        elif metadata is not None and stat_mode.S_ISLNK(metadata.st_mode):
            kind = "symlink"
        elif metadata is not None and stat_mode.S_ISDIR(metadata.st_mode):
            kind = "directory"
        elif metadata is not None and stat_mode.S_ISREG(metadata.st_mode):
            kind = "file"
        elif exists:
            kind = "other"
        else:
            kind = "missing"
        path_result = {
            "path": rel_path,
            "exists": exists,
            "kind": kind,
            "matched_rules": matched_rules,
            "included": exists and kind in {"file", "directory"} and not final_ignored,
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

        if kind not in {"file", "directory"}:
            violations.append(
                {
                    "path": rel_path,
                    "source": "required_paths",
                    "line": None,
                    "pattern": None,
                    "reason": "required_path_not_regular",
                    "message": f"required path is not a regular file or directory: {rel_path} ({kind})",
                }
            )

        if final_ignored and final_rule is not None:
            location = final_rule.source
            if final_rule.line is not None:
                location = f"{location}:{final_rule.line}"
            violations.append(
                {
                    "path": rel_path,
                    "source": final_rule.source,
                    "line": final_rule.line,
                    "pattern": final_rule.pattern,
                    "reason": "required_path_excluded",
                    "message": (
                        f"{rel_path} is excluded by {location} "
                        f"pattern {final_rule.pattern!r}"
                    ),
                }
            )

    return required_results, violations


def build_report(
    repo_root: Path,
    ignore_file: Path,
    config_file: Path,
    required_paths: list[str],
) -> dict[str, Any]:
    config_rules, config_errors = load_config_rules(config_file)
    ignore_rules, ignore_errors = load_ignore_rules(ignore_file)
    rules = config_rules + mandatory_rch_rules() + ignore_rules
    required_results, violations = evaluate_required_paths(repo_root, rules, required_paths)

    for error in config_errors:
        violations.append(
            {
                "path": str(config_file),
                "source": ".rch/config.toml",
                "line": None,
                "pattern": None,
                "reason": "config_file_error",
                "message": error,
            }
        )
    for error in ignore_errors:
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
        "config_file": str(config_file),
        "required_paths": required_results,
        "violations": violations,
        "summary": {
            "required_path_count": len(required_results),
            "violation_count": len(violations),
        },
    }


def print_text_report(report: dict[str, Any]) -> None:
    if report["mode"] == "postcondition-baseline":
        print(
            "RCH artifact sync postcondition baseline: "
            f"{report['status'].upper()}"
        )
        for item in report["generated_artifacts"]:
            snapshot = item["snapshot"]
            print(f"- {item['path']}: {snapshot['kind']}")
        if report["violations"]:
            print("\nViolations:")
            for violation in report["violations"]:
                print(f"- {violation['message']}")
                print(f"  action: {violation['recommended_action']}")
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
        "--config-file",
        default=None,
        help="Path to project RCH config. Defaults to <repo-root>/.rch/config.toml.",
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
    parser.add_argument(
        "--source-commit",
        help="Optional full source commit identity to bind into both postcondition phases.",
    )
    parser.add_argument(
        "--correlation-id",
        help="Optional run correlation identity to bind into both postcondition phases.",
    )
    parser.add_argument(
        "--command-digest",
        help="Optional command digest to bind into both postcondition phases.",
    )
    parser.add_argument("--json", action="store_true", help="Emit machine-readable JSON.")
    return parser.parse_args(argv)


def requested_invocation_identity(args: argparse.Namespace) -> dict[str, str] | None:
    values = {
        "source_commit": args.source_commit,
        "correlation_id": args.correlation_id,
        "command_digest": args.command_digest,
    }
    supplied = {key: value for key, value in values.items() if value}
    if supplied and len(supplied) != len(values):
        missing = sorted(set(values) - set(supplied))
        raise ValueError(
            "postcondition invocation identity requires all of --source-commit, "
            f"--correlation-id, and --command-digest; missing={missing}"
        )
    return supplied or None


def build_cli_error_report(repo_root: Path, message: str) -> dict[str, Any]:
    return {
        "schema": SCHEMA,
        "mode": "postcondition",
        "status": "fail",
        "repo_root": str(repo_root),
        "postconditions": [],
        "violations": [
            {
                "path": None,
                "source": "arguments",
                "line": None,
                "pattern": None,
                "reason": "invalid_postcondition_arguments",
                "message": message,
                "recommended_action": POSTCONDITION_ACTION,
            }
        ],
        "summary": {
            "generated_artifact_count": 0,
            "updated_count": 0,
            "unchanged_count": 0,
            "violation_count": 1,
        },
    }


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    repo_root = Path(args.repo_root).resolve()
    ignore_file = Path(args.ignore_file).resolve() if args.ignore_file else repo_root / ".rchignore"
    config_file = (
        Path(args.config_file).resolve()
        if args.config_file
        else repo_root / ".rch/config.toml"
    )
    required_paths = args.required_paths or list(DEFAULT_REQUIRED_PATHS)

    if args.mode == "postcondition":
        try:
            invocation_identity = requested_invocation_identity(args)
        except ValueError as exc:
            report = build_cli_error_report(repo_root, str(exc))
            invocation_identity = None
        else:
            report = None
        generated_artifacts = list(args.generated_artifacts)
        if report is not None:
            pass
        elif args.write_before_manifest is not None and args.before_manifest is not None:
            report = build_cli_error_report(
                repo_root,
                "--write-before-manifest and --before-manifest are mutually exclusive",
            )
        elif args.write_before_manifest is not None:
            if generated_artifacts:
                report = build_postcondition_baseline(
                    repo_root, generated_artifacts, invocation_identity
                )
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
            try:
                args.write_before_manifest.parent.mkdir(parents=True, exist_ok=True)
                args.write_before_manifest.write_text(
                    json.dumps(report, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
            except OSError as exc:
                append_before_manifest_write_error(report, args.write_before_manifest, exc)
        elif args.before_manifest is None:
            report = build_missing_before_manifest_report(repo_root, generated_artifacts)
        else:
            report = build_postcondition_report(
                repo_root,
                generated_artifacts,
                Path(os.path.abspath(os.path.normpath(str(args.before_manifest)))),
                invocation_identity,
            )
    else:
        report = build_report(repo_root, ignore_file, config_file, required_paths)

    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print_text_report(report)
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
