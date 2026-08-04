#!/usr/bin/env python3
"""Validate docs/traceability_matrix.json for CI governance checks.

This guard enforces that each requirement listed in the traceability matrix has
non-empty coverage in all CI-required categories and that referenced paths are
well-formed and resolvable (unless explicitly marked as generated_by_ci).

Stale-mapping detection (added by bd-k5q5.7.12):
- Every test file on disk must be classified in suite_classification.toml.
- Every suite_classification entry must exist on disk (no phantom entries).
- Every test path in the matrix must be classified.
- Classified test files not traced to any requirement produce warnings.

Usage:
  python3 scripts/check_traceability_matrix.py
  python3 scripts/check_traceability_matrix.py --self-test
"""

from __future__ import annotations

import argparse
from contextlib import contextmanager, redirect_stdout
import fnmatch
import glob
from io import StringIO
import json
import subprocess
import sys
from tempfile import TemporaryDirectory
import tomllib
from pathlib import Path
from typing import Any, Callable, Iterator

REPO_ROOT = Path(__file__).resolve().parent.parent
MATRIX_PATH = REPO_ROOT / "docs" / "traceability_matrix.json"
E2E_SCENARIO_MATRIX_PATH = REPO_ROOT / "docs" / "e2e_scenario_matrix.json"
ARTIFACT_INVENTORY_PATH = REPO_ROOT / "docs" / "evidence" / "high-value-suite-artifact-inventory.json"
SUITE_TOML_PATH = REPO_ROOT / "tests" / "suite_classification.toml"
MIN_REQUIRED_CATEGORIES = ("unit_tests", "e2e_scripts", "evidence_logs")
REQUIRED_E2E_SUITE_ARTIFACTS = ("output.log", "result.json", "test-log.jsonl", "artifact-index.jsonl")
REQUIRED_E2E_RUN_ARTIFACTS = ("summary.json", "environment.json", "evidence_contract.json")
REQUIRED_ARTIFACT_INVENTORY_AREAS = {
    "provider_streaming",
    "sessions",
    "extensions",
    "resource_scheduler_admission",
    "rpc_tui_e2e",
    "perf_report_generators",
    "security_scenarios",
}
ARTIFACT_INVENTORY_SCHEMA = "pi.traceability.high_value_suite_artifact_inventory.v1"
ALLOWED_E2E_ROW_STATUSES = {"covered", "waived", "planned"}
ALLOWED_E2E_SCENARIO_MATRIX_SCHEMAS = {
    "pi.e2e.scenario_matrix.v1",
    "pi.e2e.scenario_matrix.v2",
}


def is_glob_pattern(path: str) -> bool:
    return any(ch in path for ch in ("*", "?", "["))


def resolve_exists(path: str) -> bool:
    if is_glob_pattern(path):
        pattern = str(REPO_ROOT / path)
        return bool(glob.glob(pattern, recursive=True))
    return (REPO_ROOT / path).exists()


def fail(errors: list[str], message: str) -> None:
    errors.append(message)


def validate_entry(
    requirement_id: str,
    category: str,
    index: int,
    entry: Any,
    errors: list[str],
) -> None:
    location = f"{requirement_id}.{category}[{index}]"
    if not isinstance(entry, dict):
        fail(errors, f"{location} must be an object")
        return

    path = entry.get("path")
    if not isinstance(path, str) or not path.strip():
        fail(errors, f"{location}.path must be a non-empty string")
        return

    generated_by_ci = bool(entry.get("generated_by_ci", False))
    if not generated_by_ci and not resolve_exists(path):
        fail(
            errors,
            f"{location}.path points to missing file/glob: {path!r} "
            "(set generated_by_ci=true for CI-produced artifacts)",
        )


def validate_requirement(
    requirement: Any,
    required_categories: list[str],
    errors: list[str],
) -> str | None:
    if not isinstance(requirement, dict):
        fail(errors, "requirements[] entries must be objects")
        return None

    requirement_id = requirement.get("id")
    if not isinstance(requirement_id, str) or not requirement_id.strip():
        fail(errors, "requirements[].id must be a non-empty string")
        return None

    title = requirement.get("title")
    if not isinstance(title, str) or not title.strip():
        fail(errors, f"{requirement_id}.title must be a non-empty string")

    acceptance_criteria = requirement.get("acceptance_criteria")
    if not isinstance(acceptance_criteria, str) or not acceptance_criteria.strip():
        fail(errors, f"{requirement_id}.acceptance_criteria must be a non-empty string")

    for category in required_categories:
        items = requirement.get(category)
        if not isinstance(items, list) or not items:
            fail(
                errors,
                f"{requirement_id}.{category} must be a non-empty array (CI policy requirement)",
            )
            continue
        for index, entry in enumerate(items):
            validate_entry(requirement_id, category, index, entry, errors)

    return requirement_id


def load_matrix(path: Path) -> Any:
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


# ── Stale-mapping detection ──────────────────────────────────────────────────


def load_suite_classification() -> dict[str, list[str]]:
    """Parse tests/suite_classification.toml → {suite_name: [file_stem, ...]}."""
    with SUITE_TOML_PATH.open("rb") as fh:
        data = tomllib.load(fh)
    result: dict[str, list[str]] = {}
    for suite_name, suite_data in data.get("suite", {}).items():
        result[suite_name] = suite_data.get("files", [])
    return result


def extract_matrix_test_stems(matrix: dict[str, Any]) -> set[str]:
    """Collect test file stems (without tests/ prefix or .rs suffix) from the matrix."""
    stems: set[str] = set()
    for req in matrix.get("requirements", []):
        for category in ("unit_tests", "e2e_scripts"):
            for entry in req.get(category, []):
                path = entry.get("path", "")
                if path.startswith("tests/") and path.endswith(".rs"):
                    stems.add(path[len("tests/") : -len(".rs")])
    return stems


def extract_test_stem(path: str) -> str | None:
    if path.startswith("tests/") and path.endswith(".rs"):
        return path[len("tests/") : -len(".rs")]
    return None


def _git_tracked_test_stems(tests_dir: Path) -> set[str] | None:
    """Return stems of git-tracked `tests/*.rs` files, or None if unavailable.

    Filtering by git-tracked-only avoids false positives from local-only repro
    files that developers keep around but are listed in `.gitignore` (e.g.,
    `tests/edit_repro.rs`). CI checkouts only see tracked files, so reconciling
    the matrix/classification against the same set keeps developer and CI runs
    in agreement.
    """

    try:
        result = subprocess.run(
            ["git", "ls-files", "--", "tests/*.rs"],
            cwd=str(REPO_ROOT),
            check=True,
            capture_output=True,
            text=True,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None

    stems: set[str] = set()
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line.startswith("tests/") or not line.endswith(".rs"):
            continue
        # Only top-level `tests/*.rs` (matches the existing glob behaviour).
        rel = line[len("tests/") : -len(".rs")]
        if "/" in rel:
            continue
        stems.add(rel)
    return stems


def _gitignored_top_level_test_stems(tests_dir: Path) -> set[str]:
    """Return top-level test stems ignored by ordered `.gitignore` rules.

    RCH worker overlays intentionally omit `.git` metadata and can retain a
    previously transferred ignored repro. This metadata-free fallback applies
    the repository's ordered wildcard and negation rules to the deliberately
    narrow `tests/*.rs` inventory instead of treating wildcard rules as if they
    did not exist.
    """

    ignore_path = REPO_ROOT / ".gitignore"
    try:
        lines = ignore_path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return set()

    ignored: set[str] = set()
    for test_path in sorted(tests_dir.glob("*.rs")):
        relative = test_path.relative_to(REPO_ROOT).as_posix()
        is_ignored = False
        for raw in lines:
            rule = raw.strip()
            if not rule or rule.startswith("#"):
                continue

            negated = rule.startswith("!")
            if negated:
                rule = rule[1:]
            elif rule.startswith(r"\!"):
                rule = rule[1:]
            if rule.startswith(r"\#"):
                rule = rule[1:]
            if not rule:
                continue

            directory_rule = rule.endswith("/")
            rule = rule.lstrip("/").rstrip("/")
            if not rule:
                continue
            if "/" in rule:
                matches = fnmatch.fnmatchcase(relative, rule)
                if directory_rule:
                    matches = relative.startswith(f"{rule}/")
            else:
                matches = fnmatch.fnmatchcase(test_path.name, rule)
            if matches:
                is_ignored = not negated

        if is_ignored:
            ignored.add(test_path.stem)
    return ignored


def check_stale_mappings(
    matrix: dict[str, Any],
    errors: list[str],
    warnings: list[str],
) -> tuple[dict[str, int], list[str]]:
    """Cross-reference traceability matrix, suite classification, and disk.

    Returns:
        (stats, untraceable_stems)
    """
    stats: dict[str, int] = {
        "on_disk": 0,
        "classified": 0,
        "matrix_traced": 0,
        "unclassified": 0,
        "phantom": 0,
        "untraceable": 0,
    }

    if not SUITE_TOML_PATH.exists():
        fail(errors, f"suite classification missing: {SUITE_TOML_PATH}")
        return stats, []

    suites = load_suite_classification()
    classified_stems: set[str] = set()
    for stems in suites.values():
        classified_stems.update(stems)

    matrix_test_stems = extract_matrix_test_stems(matrix)

    # On-disk test files. Only count git-tracked files so that local-only
    # developer repro files (gitignored) don't trigger spurious classification
    # mismatches between developer machines and CI checkouts.
    tests_dir = REPO_ROOT / "tests"
    on_disk_stems: set[str] = set()
    tracked_stems = _git_tracked_test_stems(tests_dir)
    if tracked_stems is None:
        # Not a git checkout (or git unavailable): fall back to filesystem scan.
        ignored_stems = _gitignored_top_level_test_stems(tests_dir)
        for f in sorted(tests_dir.glob("*.rs")):
            if f.stem not in ignored_stems:
                on_disk_stems.add(f.stem)
    else:
        on_disk_stems = tracked_stems

    stats["on_disk"] = len(on_disk_stems)
    stats["classified"] = len(classified_stems)
    stats["matrix_traced"] = len(matrix_test_stems)

    # 1. Unclassified: on disk but not in suite_classification.toml.
    unclassified = on_disk_stems - classified_stems
    stats["unclassified"] = len(unclassified)
    for stem in sorted(unclassified):
        fail(errors, f"tests/{stem}.rs is on disk but missing from suite_classification.toml")

    # 2. Phantom: in suite_classification but not on disk.
    phantom = classified_stems - on_disk_stems
    stats["phantom"] = len(phantom)
    for stem in sorted(phantom):
        fail(errors, f"suite_classification.toml lists '{stem}' but tests/{stem}.rs does not exist")

    # 3. Matrix references test files not in suite_classification.
    matrix_not_classified = matrix_test_stems - classified_stems
    for stem in sorted(matrix_not_classified):
        fail(
            errors,
            f"traceability matrix references tests/{stem}.rs "
            "but it is not in suite_classification.toml",
        )

    # 4. Classified test files not traced to any requirement (warning, not error).
    untraceable = classified_stems - matrix_test_stems
    stats["untraceable"] = len(untraceable)
    untraceable_sorted = sorted(untraceable)
    for stem in untraceable_sorted:
        warnings.append(f"tests/{stem}.rs is classified but not traced to any requirement")

    return stats, untraceable_sorted


def validate_e2e_scenario_matrix(
    errors: list[str],
    warnings: list[str],
) -> dict[str, float | int]:
    """Validate canonical E2E scenario matrix schema + drift against suite.e2e."""
    stats: dict[str, float | int] = {
        "rows": 0,
        "planned_rows": 0,
        "waived_rows": 0,
        "classified_e2e": 0,
        "covered_e2e_suites": 0,
        "coverage_pct": 0.0,
    }

    if not E2E_SCENARIO_MATRIX_PATH.exists():
        fail(errors, f"missing canonical E2E scenario matrix: {E2E_SCENARIO_MATRIX_PATH}")
        return stats

    try:
        with E2E_SCENARIO_MATRIX_PATH.open("r", encoding="utf-8") as fh:
            matrix = json.load(fh)
    except json.JSONDecodeError as exc:
        fail(errors, f"invalid JSON in {E2E_SCENARIO_MATRIX_PATH}: {exc}")
        return stats

    if not isinstance(matrix, dict):
        fail(errors, "docs/e2e_scenario_matrix.json root must be an object")
        return stats

    for key in ("schema", "bead_id", "updated_at", "ci_policy", "rows"):
        if key not in matrix:
            fail(errors, f"docs/e2e_scenario_matrix.json missing top-level key: {key}")

    if matrix.get("schema") not in ALLOWED_E2E_SCENARIO_MATRIX_SCHEMAS:
        fail(
            errors,
            "docs/e2e_scenario_matrix.json schema must be one of "
            f"{sorted(ALLOWED_E2E_SCENARIO_MATRIX_SCHEMAS)}",
        )

    ci_policy = matrix.get("ci_policy")
    if not isinstance(ci_policy, dict):
        fail(errors, "docs/e2e_scenario_matrix.json ci_policy must be an object")
        ci_policy = {}

    consumed_by = ci_policy.get("consumed_by")
    if not isinstance(consumed_by, list) or not consumed_by:
        fail(errors, "e2e scenario matrix ci_policy.consumed_by must be a non-empty array")
    else:
        for consumer in (
            "scripts/check_traceability_matrix.py",
            "tests/ci_full_suite_gate.rs",
        ):
            if consumer not in consumed_by:
                fail(errors, f"e2e scenario matrix ci_policy.consumed_by missing {consumer}")

    required_suite_artifacts = ci_policy.get("required_suite_artifacts")
    if not isinstance(required_suite_artifacts, list) or not required_suite_artifacts:
        fail(errors, "e2e scenario matrix ci_policy.required_suite_artifacts must be non-empty")
        required_suite_artifacts = list(REQUIRED_E2E_SUITE_ARTIFACTS)
    for artifact in REQUIRED_E2E_SUITE_ARTIFACTS:
        if artifact not in required_suite_artifacts:
            fail(
                errors,
                f"e2e scenario matrix ci_policy.required_suite_artifacts missing {artifact!r}",
            )

    required_run_artifacts = ci_policy.get("required_run_artifacts")
    if not isinstance(required_run_artifacts, list) or not required_run_artifacts:
        fail(errors, "e2e scenario matrix ci_policy.required_run_artifacts must be non-empty")
        required_run_artifacts = list(REQUIRED_E2E_RUN_ARTIFACTS)
    for artifact in REQUIRED_E2E_RUN_ARTIFACTS:
        if artifact not in required_run_artifacts:
            fail(
                errors,
                f"e2e scenario matrix ci_policy.required_run_artifacts missing {artifact!r}",
            )

    min_cov = ci_policy.get("min_e2e_suite_matrix_coverage_pct", 0)
    if not isinstance(min_cov, (int, float)):
        fail(errors, "ci_policy.min_e2e_suite_matrix_coverage_pct must be numeric")
        min_cov = 0.0
    else:
        min_cov = float(min_cov)
        if min_cov < 0.0 or min_cov > 100.0:
            fail(errors, "ci_policy.min_e2e_suite_matrix_coverage_pct must be within [0,100]")
            min_cov = 0.0

    rows = matrix.get("rows")
    if not isinstance(rows, list) or not rows:
        fail(errors, "docs/e2e_scenario_matrix.json rows must be a non-empty array")
        rows = []
    stats["rows"] = len(rows)

    suites = load_suite_classification()
    classified_e2e = set(suites.get("e2e", []))
    stats["classified_e2e"] = len(classified_e2e)

    referenced_suites: set[str] = set()
    for index, row in enumerate(rows):
        location = f"rows[{index}]"
        if not isinstance(row, dict):
            fail(errors, f"{location} must be an object")
            continue

        for key in (
            "workflow_id",
            "workflow_class",
            "workflow_title",
            "status",
            "owner",
            "provider_families",
            "expected_artifacts",
            "replay_command",
        ):
            if key not in row:
                fail(errors, f"{location}.{key} is required")

        status = row.get("status")
        if not isinstance(status, str) or status not in ALLOWED_E2E_ROW_STATUSES:
            fail(
                errors,
                f"{location}.status must be one of {sorted(ALLOWED_E2E_ROW_STATUSES)}",
            )
            continue

        owner = row.get("owner")
        if not isinstance(owner, str) or not owner.strip():
            fail(errors, f"{location}.owner must be a non-empty string")

        provider_families = row.get("provider_families")
        if not isinstance(provider_families, list) or not provider_families:
            fail(errors, f"{location}.provider_families must be a non-empty array")
        else:
            for pf in provider_families:
                if not isinstance(pf, str) or not pf.strip():
                    fail(errors, f"{location}.provider_families entries must be non-empty strings")

        expected_artifacts = row.get("expected_artifacts")
        if not isinstance(expected_artifacts, list) or not expected_artifacts:
            fail(errors, f"{location}.expected_artifacts must be a non-empty array")
            expected_artifacts = []

        for artifact in (*REQUIRED_E2E_SUITE_ARTIFACTS, *REQUIRED_E2E_RUN_ARTIFACTS):
            if artifact not in expected_artifacts:
                fail(errors, f"{location}.expected_artifacts missing {artifact!r}")

        if status == "planned":
            stats["planned_rows"] = int(stats["planned_rows"]) + 1
            planned_suite_ids = row.get("planned_suite_ids")
            if not isinstance(planned_suite_ids, list) or not planned_suite_ids:
                fail(errors, f"{location}.planned_suite_ids must be non-empty for planned rows")
            planned_issue_id = row.get("planned_issue_id")
            if not isinstance(planned_issue_id, str) or not planned_issue_id.strip():
                fail(errors, f"{location}.planned_issue_id must be non-empty for planned rows")
            continue

        suite_ids = row.get("suite_ids")
        if not isinstance(suite_ids, list) or not suite_ids:
            fail(errors, f"{location}.suite_ids must be non-empty for covered/waived rows")
            suite_ids = []

        test_paths = row.get("test_paths")
        if not isinstance(test_paths, list) or not test_paths:
            fail(errors, f"{location}.test_paths must be non-empty for covered/waived rows")
            test_paths = []

        path_stems: set[str] = set()
        for path in test_paths:
            if not isinstance(path, str) or not path.strip():
                fail(errors, f"{location}.test_paths entries must be non-empty strings")
                continue
            if not resolve_exists(path):
                fail(errors, f"{location}.test_paths references missing file: {path}")
                continue
            stem = extract_test_stem(path)
            if not stem:
                fail(errors, f"{location}.test_paths must point to tests/*.rs files (got {path!r})")
                continue
            path_stems.add(stem)
            if stem not in classified_e2e:
                fail(
                    errors,
                    f"{location}.test_paths references tests/{stem}.rs not listed in [suite.e2e]",
                )
            referenced_suites.add(stem)

        for suite_id in suite_ids:
            if not isinstance(suite_id, str) or not suite_id.strip():
                fail(errors, f"{location}.suite_ids entries must be non-empty strings")
                continue
            if suite_id not in classified_e2e:
                fail(errors, f"{location}.suite_ids includes unclassified e2e suite: {suite_id}")
            referenced_suites.add(suite_id)

        if path_stems and isinstance(suite_ids, list):
            suite_id_set = {suite for suite in suite_ids if isinstance(suite, str)}
            if path_stems != suite_id_set:
                fail(
                    errors,
                    f"{location} suite_ids must match test_paths stems (suite_ids={sorted(suite_id_set)}, stems={sorted(path_stems)})",
                )

        if status == "waived":
            stats["waived_rows"] = int(stats["waived_rows"]) + 1
            waiver_reason = row.get("waiver_reason")
            if not isinstance(waiver_reason, str) or not waiver_reason.strip():
                fail(errors, f"{location}.waiver_reason must be non-empty for waived rows")
            waiver_issue_id = row.get("waiver_issue_id")
            if not isinstance(waiver_issue_id, str) or not waiver_issue_id.strip():
                fail(errors, f"{location}.waiver_issue_id must be non-empty for waived rows")

    if stats["classified_e2e"] > 0:
        covered = len(referenced_suites)
        stats["covered_e2e_suites"] = covered
        coverage_pct = (covered / stats["classified_e2e"]) * 100.0
        stats["coverage_pct"] = coverage_pct
        missing = sorted(classified_e2e - referenced_suites)
        if coverage_pct < min_cov:
            sample = ", ".join(missing[:10]) if missing else "(none)"
            fail(
                errors,
                "e2e scenario matrix coverage below threshold: "
                f"{coverage_pct:.2f}% < {min_cov:.2f}% "
                f"(covered={covered}, classified={stats['classified_e2e']}). "
                f"Sample missing suites: {sample}",
            )
        if missing:
            fail(
                errors,
                "e2e scenario matrix missing classified [suite.e2e] entries: "
                + ", ".join(missing),
            )
    else:
        warnings.append("suite.e2e is empty; e2e scenario matrix coverage checks skipped")

    return stats


def matrix_references_evidence_path(matrix: dict[str, Any], required_path: str) -> bool:
    for requirement in matrix.get("requirements", []):
        if not isinstance(requirement, dict):
            continue
        for entry in requirement.get("evidence_logs", []):
            if not isinstance(entry, dict):
                continue
            if entry.get("path") == required_path:
                return True
    return False


def validate_high_value_artifact_inventory(
    matrix: dict[str, Any],
    errors: list[str],
) -> dict[str, int]:
    """Validate the machine-readable inventory that unblocks traceability repair."""
    stats = {
        "selected_suites": 0,
        "coverage_areas": 0,
        "artifact_refs": 0,
    }

    if not ARTIFACT_INVENTORY_PATH.exists():
        fail(errors, f"missing high-value suite artifact inventory: {ARTIFACT_INVENTORY_PATH}")
        return stats

    try:
        with ARTIFACT_INVENTORY_PATH.open("r", encoding="utf-8") as fh:
            inventory = json.load(fh)
    except json.JSONDecodeError as exc:
        fail(errors, f"invalid JSON in {ARTIFACT_INVENTORY_PATH}: {exc}")
        return stats

    if not isinstance(inventory, dict):
        fail(errors, "high-value suite artifact inventory root must be an object")
        return stats

    if inventory.get("schema") != ARTIFACT_INVENTORY_SCHEMA:
        fail(
            errors,
            "high-value suite artifact inventory schema must be "
            f"{ARTIFACT_INVENTORY_SCHEMA!r}",
        )

    generated_at = inventory.get("generated_at")
    if not isinstance(generated_at, str) or not generated_at.strip():
        fail(errors, "high-value suite artifact inventory generated_at must be non-empty")

    selected_suites = inventory.get("selected_suites")
    if not isinstance(selected_suites, list) or not selected_suites:
        fail(errors, "high-value suite artifact inventory selected_suites must be non-empty")
        selected_suites = []
    stats["selected_suites"] = len(selected_suites)

    seen_areas: set[str] = set()
    for index, suite in enumerate(selected_suites):
        location = f"selected_suites[{index}]"
        if not isinstance(suite, dict):
            fail(errors, f"{location} must be an object")
            continue

        suite_id = suite.get("id")
        if not isinstance(suite_id, str) or not suite_id.strip():
            fail(errors, f"{location}.id must be a non-empty string")

        coverage_area = suite.get("coverage_area")
        if not isinstance(coverage_area, str) or not coverage_area.strip():
            fail(errors, f"{location}.coverage_area must be a non-empty string")
        else:
            seen_areas.add(coverage_area)

        for field in ("suite_ids", "test_paths", "schema_tags"):
            value = suite.get(field)
            if not isinstance(value, list) or not value:
                fail(errors, f"{location}.{field} must be a non-empty array")
                continue
            for item in value:
                if not isinstance(item, str) or not item.strip():
                    fail(errors, f"{location}.{field} entries must be non-empty strings")

        tmpdir_policy = suite.get("tmpdir_policy")
        if not isinstance(tmpdir_policy, str) or not tmpdir_policy.strip():
            fail(errors, f"{location}.tmpdir_policy must be a non-empty string")

        replay = suite.get("deterministic_replay_command")
        if not isinstance(replay, str) or not replay.strip():
            fail(errors, f"{location}.deterministic_replay_command must be non-empty")

        artifact_refs = suite.get("artifact_refs")
        if not isinstance(artifact_refs, list) or not artifact_refs:
            fail(errors, f"{location}.artifact_refs must be a non-empty array")
            artifact_refs = []
        stats["artifact_refs"] += len(artifact_refs)

        for artifact_index, artifact in enumerate(artifact_refs):
            artifact_location = f"{location}.artifact_refs[{artifact_index}]"
            if not isinstance(artifact, dict):
                fail(errors, f"{artifact_location} must be an object")
                continue
            path = artifact.get("path")
            if not isinstance(path, str) or not path.strip():
                fail(errors, f"{artifact_location}.path must be a non-empty string")
                continue
            if not bool(artifact.get("generated_by_ci", False)) and not resolve_exists(path):
                fail(
                    errors,
                    f"{artifact_location}.path points to missing file/glob: {path!r}",
                )
            kind = artifact.get("kind")
            if not isinstance(kind, str) or not kind.strip():
                fail(errors, f"{artifact_location}.kind must be a non-empty string")

    stats["coverage_areas"] = len(seen_areas)
    missing_areas = REQUIRED_ARTIFACT_INVENTORY_AREAS - seen_areas
    if missing_areas:
        fail(
            errors,
            "high-value suite artifact inventory missing coverage areas: "
            + ", ".join(sorted(missing_areas)),
        )

    relative_path = "docs/evidence/high-value-suite-artifact-inventory.json"
    if not matrix_references_evidence_path(matrix, relative_path):
        fail(
            errors,
            "traceability_matrix evidence_logs must reference "
            f"{relative_path}",
        )

    return stats


def set_repo_paths(repo_root: Path) -> None:
    global REPO_ROOT, MATRIX_PATH, E2E_SCENARIO_MATRIX_PATH
    global ARTIFACT_INVENTORY_PATH, SUITE_TOML_PATH

    REPO_ROOT = repo_root
    MATRIX_PATH = REPO_ROOT / "docs" / "traceability_matrix.json"
    E2E_SCENARIO_MATRIX_PATH = REPO_ROOT / "docs" / "e2e_scenario_matrix.json"
    ARTIFACT_INVENTORY_PATH = (
        REPO_ROOT / "docs" / "evidence" / "high-value-suite-artifact-inventory.json"
    )
    SUITE_TOML_PATH = REPO_ROOT / "tests" / "suite_classification.toml"


@contextmanager
def fixture_repo_root(repo_root: Path) -> Iterator[None]:
    old_paths = (
        REPO_ROOT,
        MATRIX_PATH,
        E2E_SCENARIO_MATRIX_PATH,
        ARTIFACT_INVENTORY_PATH,
        SUITE_TOML_PATH,
    )
    set_repo_paths(repo_root)
    try:
        yield
    finally:
        (
            globals()["REPO_ROOT"],
            globals()["MATRIX_PATH"],
            globals()["E2E_SCENARIO_MATRIX_PATH"],
            globals()["ARTIFACT_INVENTORY_PATH"],
            globals()["SUITE_TOML_PATH"],
        ) = old_paths


def write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def fixture_traceability_matrix() -> dict[str, Any]:
    return {
        "schema_version": "fixture.v1",
        "program_issue_id": "bd-fixture",
        "program_title": "Fixture traceability program",
        "updated_at": "2026-05-19T00:00:00Z",
        "ci_policy": {
            "required_categories": ["unit_tests", "e2e_scripts", "evidence_logs"],
            "min_classified_trace_coverage_pct": 100,
        },
        "requirements": [
            {
                "id": "REQ-FIXTURE-1",
                "title": "Fixture requirement",
                "acceptance_criteria": "The fixture guard validates all required policy surfaces.",
                "unit_tests": [{"path": "tests/unit_one.rs"}],
                "e2e_scripts": [{"path": "tests/e2e_one.rs"}],
                "evidence_logs": [
                    {"path": "docs/evidence/high-value-suite-artifact-inventory.json"},
                ],
            },
        ],
    }


def fixture_e2e_matrix() -> dict[str, Any]:
    expected_artifacts = list(REQUIRED_E2E_SUITE_ARTIFACTS + REQUIRED_E2E_RUN_ARTIFACTS)
    return {
        "schema": "pi.e2e.scenario_matrix.v2",
        "bead_id": "bd-fixture",
        "updated_at": "2026-05-19T00:00:00Z",
        "ci_policy": {
            "consumed_by": [
                "scripts/check_traceability_matrix.py",
                "tests/ci_full_suite_gate.rs",
            ],
            "required_suite_artifacts": list(REQUIRED_E2E_SUITE_ARTIFACTS),
            "required_run_artifacts": list(REQUIRED_E2E_RUN_ARTIFACTS),
            "min_e2e_suite_matrix_coverage_pct": 100,
        },
        "rows": [
            {
                "workflow_id": "fixture-e2e",
                "workflow_class": "fixture",
                "workflow_title": "Fixture E2E",
                "status": "covered",
                "owner": "fixture",
                "provider_families": ["fixture"],
                "expected_artifacts": expected_artifacts,
                "replay_command": "cargo test --test e2e_one",
                "suite_ids": ["e2e_one"],
                "test_paths": ["tests/e2e_one.rs"],
            },
        ],
    }


def fixture_artifact_inventory() -> dict[str, Any]:
    selected_suites = []
    for area in sorted(REQUIRED_ARTIFACT_INVENTORY_AREAS):
        selected_suites.append(
            {
                "id": f"{area}-fixture",
                "coverage_area": area,
                "suite_ids": ["e2e_one"],
                "test_paths": ["tests/e2e_one.rs"],
                "schema_tags": [f"pi.fixture.{area}.v1"],
                "tmpdir_policy": "uses disposable fixture directories",
                "deterministic_replay_command": "cargo test --test e2e_one",
                "artifact_refs": [
                    {
                        "path": f"target/fixture/{area}.jsonl",
                        "kind": "jsonl",
                        "generated_by_ci": True,
                    },
                ],
            }
        )
    return {
        "schema": ARTIFACT_INVENTORY_SCHEMA,
        "generated_at": "2026-05-19T00:00:00Z",
        "selected_suites": selected_suites,
    }


def write_fixture_repo(repo_root: Path) -> None:
    (repo_root / "tests").mkdir(parents=True, exist_ok=True)
    (repo_root / "tests" / "unit_one.rs").write_text("// fixture unit test\n", encoding="utf-8")
    (repo_root / "tests" / "e2e_one.rs").write_text("// fixture e2e test\n", encoding="utf-8")
    (repo_root / "tests" / "suite_classification.toml").write_text(
        "[suite.unit]\nfiles = [\"unit_one\"]\n\n[suite.e2e]\nfiles = [\"e2e_one\"]\n",
        encoding="utf-8",
    )
    write_json(repo_root / "docs" / "traceability_matrix.json", fixture_traceability_matrix())
    write_json(repo_root / "docs" / "e2e_scenario_matrix.json", fixture_e2e_matrix())
    write_json(
        repo_root / "docs" / "evidence" / "high-value-suite-artifact-inventory.json",
        fixture_artifact_inventory(),
    )


def run_fixture_check(repo_root: Path) -> tuple[int, str]:
    output = StringIO()
    with fixture_repo_root(repo_root), redirect_stdout(output):
        code = run_check()
    return code, output.getvalue()


def assert_self_test(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def read_fixture_json(repo_root: Path, relative_path: str) -> dict[str, Any]:
    with (repo_root / relative_path).open("r", encoding="utf-8") as fh:
        data = json.load(fh)
    if not isinstance(data, dict):
        raise AssertionError(f"{relative_path} fixture root must be an object")
    return data


def run_self_test_case(name: str, mutate: Callable[[Path], None], expected: str) -> None:
    with TemporaryDirectory(prefix=f"traceability-{name}-") as tmp:
        repo_root = Path(tmp)
        write_fixture_repo(repo_root)
        mutate(repo_root)
        code, output = run_fixture_check(repo_root)
    assert_self_test(code == 1, f"{name} should fail")
    assert_self_test(expected in output, f"{name} output should include {expected!r}")


def run_self_test() -> int:
    with TemporaryDirectory(prefix="traceability-pass-") as tmp:
        repo_root = Path(tmp)
        write_fixture_repo(repo_root)
        code, output = run_fixture_check(repo_root)
    assert_self_test(code == 0, "valid fixture should pass")
    assert_self_test("TRACEABILITY CHECK PASSED" in output, "pass output should report success")

    with TemporaryDirectory(prefix="traceability-gitignore-wildcard-") as tmp:
        repo_root = Path(tmp)
        write_fixture_repo(repo_root)
        (repo_root / "tests" / "ignored_repro.rs").write_text(
            "// ignored fixture repro\n", encoding="utf-8"
        )
        (repo_root / ".gitignore").write_text(
            "/tests/*.rs\n!/tests/unit_one.rs\n!/tests/e2e_one.rs\n",
            encoding="utf-8",
        )
        code, output = run_fixture_check(repo_root)
    assert_self_test(code == 0, "wildcard-ignored repro should not enter test inventory")
    assert_self_test(
        "TRACEABILITY CHECK PASSED" in output,
        "wildcard-ignore fixture should report success",
    )

    def add_negated_unclassified_test(repo_root: Path) -> None:
        (repo_root / "tests" / "reincluded.rs").write_text(
            "// explicitly reincluded fixture\n", encoding="utf-8"
        )
        (repo_root / ".gitignore").write_text(
            "/tests/*.rs\n"
            "!/tests/unit_one.rs\n"
            "!/tests/e2e_one.rs\n"
            "!/tests/reincluded.rs\n",
            encoding="utf-8",
        )

    run_self_test_case(
        "gitignore-negation",
        add_negated_unclassified_test,
        "tests/reincluded.rs is on disk but missing from suite_classification.toml",
    )

    def remove_required_category(repo_root: Path) -> None:
        matrix = read_fixture_json(repo_root, "docs/traceability_matrix.json")
        matrix["requirements"][0].pop("evidence_logs")
        write_json(repo_root / "docs" / "traceability_matrix.json", matrix)

    run_self_test_case("required-category", remove_required_category, "evidence_logs")

    def point_to_missing_path(repo_root: Path) -> None:
        matrix = read_fixture_json(repo_root, "docs/traceability_matrix.json")
        matrix["requirements"][0]["unit_tests"][0]["path"] = "tests/missing.rs"
        write_json(repo_root / "docs" / "traceability_matrix.json", matrix)

    run_self_test_case("missing-path", point_to_missing_path, "missing file/glob")

    def add_unclassified_test(repo_root: Path) -> None:
        (repo_root / "tests" / "orphan.rs").write_text("// fixture orphan\n", encoding="utf-8")

    run_self_test_case("suite-drift", add_unclassified_test, "missing from suite_classification")

    def add_uncovered_e2e_suite(repo_root: Path) -> None:
        (repo_root / "tests" / "e2e_two.rs").write_text("// fixture e2e\n", encoding="utf-8")
        (repo_root / "tests" / "suite_classification.toml").write_text(
            "[suite.unit]\nfiles = [\"unit_one\"]\n\n"
            "[suite.e2e]\nfiles = [\"e2e_one\", \"e2e_two\"]\n",
            encoding="utf-8",
        )
        matrix = read_fixture_json(repo_root, "docs/traceability_matrix.json")
        matrix["requirements"][0]["e2e_scripts"].append({"path": "tests/e2e_two.rs"})
        write_json(repo_root / "docs" / "traceability_matrix.json", matrix)

    run_self_test_case("e2e-coverage", add_uncovered_e2e_suite, "missing classified [suite.e2e]")

    def remove_inventory_area(repo_root: Path) -> None:
        inventory = read_fixture_json(
            repo_root,
            "docs/evidence/high-value-suite-artifact-inventory.json",
        )
        inventory["selected_suites"] = inventory["selected_suites"][1:]
        write_json(
            repo_root / "docs" / "evidence" / "high-value-suite-artifact-inventory.json",
            inventory,
        )

    run_self_test_case("artifact-inventory", remove_inventory_area, "missing coverage areas")

    print("Traceability matrix self-test passed.")
    return 0


# ── main ─────────────────────────────────────────────────────────────────────


def run_check() -> int:
    errors: list[str] = []
    warnings: list[str] = []

    if not MATRIX_PATH.exists():
        print(f"TRACEABILITY CHECK FAILED: missing {MATRIX_PATH}")
        return 1

    try:
        matrix = load_matrix(MATRIX_PATH)
    except json.JSONDecodeError as exc:
        print(f"TRACEABILITY CHECK FAILED: invalid JSON in {MATRIX_PATH}: {exc}")
        return 1

    if not isinstance(matrix, dict):
        print("TRACEABILITY CHECK FAILED: matrix root must be a JSON object")
        return 1

    for key in ("schema_version", "program_issue_id", "program_title", "updated_at", "ci_policy", "requirements"):
        if key not in matrix:
            fail(errors, f"missing top-level key: {key}")

    ci_policy = matrix.get("ci_policy", {})
    if not isinstance(ci_policy, dict):
        fail(errors, "ci_policy must be an object")
        ci_policy = {}

    required_categories_raw = ci_policy.get("required_categories", [])
    if not isinstance(required_categories_raw, list) or not required_categories_raw:
        fail(errors, "ci_policy.required_categories must be a non-empty array")
        required_categories = list(MIN_REQUIRED_CATEGORIES)
    else:
        required_categories = []
        for category in required_categories_raw:
            if not isinstance(category, str) or not category.strip():
                fail(errors, "ci_policy.required_categories entries must be non-empty strings")
                continue
            required_categories.append(category)

    for minimum in MIN_REQUIRED_CATEGORIES:
        if minimum not in required_categories:
            fail(
                errors,
                f"ci_policy.required_categories must include {minimum!r}",
            )

    requirements = matrix.get("requirements")
    if not isinstance(requirements, list) or not requirements:
        fail(errors, "requirements must be a non-empty array")
        requirements = []

    seen_ids: set[str] = set()
    for requirement in requirements:
        requirement_id = validate_requirement(requirement, required_categories, errors)
        if not requirement_id:
            continue
        if requirement_id in seen_ids:
            fail(errors, f"duplicate requirement id: {requirement_id}")
        seen_ids.add(requirement_id)

    min_trace_coverage_pct = ci_policy.get("min_classified_trace_coverage_pct")
    if min_trace_coverage_pct is None:
        fail(errors, "ci_policy.min_classified_trace_coverage_pct must be set")
        min_trace_coverage_pct = 0.0
    elif not isinstance(min_trace_coverage_pct, (int, float)):
        fail(errors, "ci_policy.min_classified_trace_coverage_pct must be numeric")
        min_trace_coverage_pct = 0.0
    elif float(min_trace_coverage_pct) < 0.0 or float(min_trace_coverage_pct) > 100.0:
        fail(errors, "ci_policy.min_classified_trace_coverage_pct must be within [0,100]")
        min_trace_coverage_pct = 0.0
    else:
        min_trace_coverage_pct = float(min_trace_coverage_pct)

    # Stale-mapping detection (bd-k5q5.7.12).
    stats, untraceable = check_stale_mappings(matrix, errors, warnings)
    if stats["classified"] > 0:
        coverage_pct = (stats["matrix_traced"] / stats["classified"]) * 100.0
        if coverage_pct < min_trace_coverage_pct:
            sample = ", ".join(f"tests/{stem}.rs" for stem in untraceable[:10]) or "(none)"
            fail(
                errors,
                "classified traceability coverage below policy threshold: "
                f"{coverage_pct:.2f}% < {min_trace_coverage_pct:.2f}% "
                f"(classified={stats['classified']}, traced={stats['matrix_traced']}). "
                f"Sample missing mappings: {sample}",
            )

    # Canonical E2E scenario matrix validation (bd-1f42.8.5.1).
    e2e_stats = validate_e2e_scenario_matrix(errors, warnings)

    # High-value JSON/JSONL artifact inventory (bd-8t27h.9).
    artifact_inventory_stats = validate_high_value_artifact_inventory(matrix, errors)

    if errors:
        print("TRACEABILITY CHECK FAILED")
        for error in errors:
            print(f"- {error}")
        if warnings:
            print(f"\nSTALENESS WARNINGS ({len(warnings)}):")
            for w in warnings:
                print(f"  - {w}")
        return 1

    summary_parts = [
        f"{len(requirements)} requirements validated",
        f"categories: {', '.join(required_categories)}",
    ]
    if stats["on_disk"]:
        coverage_pct = (
            (stats["matrix_traced"] / stats["classified"]) * 100.0
            if stats["classified"] > 0
            else 0.0
        )
        summary_parts.append(
            f"staleness: {stats['on_disk']} on-disk, "
            f"{stats['classified']} classified, "
            f"{stats['matrix_traced']} traced"
        )
        summary_parts.append(
            f"trace coverage: {coverage_pct:.2f}% "
            f"(min {min_trace_coverage_pct:.2f}%)"
        )
    if e2e_stats["classified_e2e"]:
        summary_parts.append(
            "e2e matrix coverage: "
            f"{int(e2e_stats['covered_e2e_suites'])}/{int(e2e_stats['classified_e2e'])} "
            f"({float(e2e_stats['coverage_pct']):.2f}%)"
        )
        summary_parts.append(
            f"e2e matrix rows: {int(e2e_stats['rows'])} "
            f"(planned={int(e2e_stats['planned_rows'])}, waived={int(e2e_stats['waived_rows'])})"
        )
    summary_parts.append(
        "artifact inventory: "
        f"{artifact_inventory_stats['selected_suites']} suites, "
        f"{artifact_inventory_stats['coverage_areas']} areas, "
        f"{artifact_inventory_stats['artifact_refs']} refs"
    )
    print(f"TRACEABILITY CHECK PASSED: {'; '.join(summary_parts)}")

    if warnings:
        print(f"\nSTALENESS WARNINGS ({len(warnings)}):")
        for w in warnings:
            print(f"  - {w}")

    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Validate docs/traceability_matrix.json and related CI evidence policy.",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run deterministic fixture-backed checks without mutating repository artifacts",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        try:
            return run_self_test()
        except AssertionError as exc:
            print("Traceability matrix self-test failed:", file=sys.stderr)
            print(f"- {exc}", file=sys.stderr)
            return 1

    return run_check()


if __name__ == "__main__":
    raise SystemExit(main())
