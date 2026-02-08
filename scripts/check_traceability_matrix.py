#!/usr/bin/env python3
"""Validate docs/traceability_matrix.json for CI governance checks.

This guard enforces that each requirement listed in the traceability matrix has
non-empty coverage in all CI-required categories and that referenced paths are
well-formed and resolvable (unless explicitly marked as generated_by_ci).
"""

from __future__ import annotations

import glob
import json
import sys
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
MATRIX_PATH = REPO_ROOT / "docs" / "traceability_matrix.json"
MIN_REQUIRED_CATEGORIES = ("unit_tests", "e2e_scripts", "evidence_logs")


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


def main() -> int:
    errors: list[str] = []

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

    if errors:
        print("TRACEABILITY CHECK FAILED")
        for error in errors:
            print(f"- {error}")
        return 1

    print(
        "TRACEABILITY CHECK PASSED: "
        f"{len(requirements)} requirements validated with categories "
        f"{', '.join(required_categories)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
