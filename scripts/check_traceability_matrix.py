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
"""

from __future__ import annotations

import glob
import json
import tomllib
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
MATRIX_PATH = REPO_ROOT / "docs" / "traceability_matrix.json"
SUITE_TOML_PATH = REPO_ROOT / "tests" / "suite_classification.toml"
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

    # On-disk test files.
    tests_dir = REPO_ROOT / "tests"
    on_disk_stems: set[str] = set()
    for f in sorted(tests_dir.glob("*.rs")):
        on_disk_stems.add(f.stem)

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


# ── main ─────────────────────────────────────────────────────────────────────


def main() -> int:
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
    print(f"TRACEABILITY CHECK PASSED: {'; '.join(summary_parts)}")

    if warnings:
        print(f"\nSTALENESS WARNINGS ({len(warnings)}):")
        for w in warnings:
            print(f"  - {w}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
