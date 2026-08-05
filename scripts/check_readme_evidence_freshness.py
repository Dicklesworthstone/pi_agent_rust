#!/usr/bin/env python3
"""Check README evidence artifact freshness for CI governance.

This guard enforces that artifact citations in README.md are fresh (<=14 days old).
Citations have the format `*(from artifact-path, run correlation-id)*` or
`*(from artifact-path, generated timestamp)*`.

If any cited artifact is missing, stale, missing the cited correlation id, or is
a performance budget summary without a complete v2 claim-readiness
authorization, the check fails to prevent stale or unverifiable evidence from
misleading users about current project capabilities.

Usage:
    python3 scripts/check_readme_evidence_freshness.py
    python3 scripts/check_readme_evidence_freshness.py --self-test

Exit codes:
    0 - All citations are fresh
    1 - One or more missing or stale citations found
    2 - Script error (missing files, parse failures, etc.)
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import NamedTuple


PERF_BUDGET_SUMMARY_SCHEMA = "pi.perf.budget_summary.v2"


class CitationCheck(NamedTuple):
    """Result of checking a single citation."""
    artifact_path: str
    correlation_id: str
    line_number: int
    claim_surface: str
    file_exists: bool
    file_mtime: datetime | None
    days_old: float | None
    is_stale: bool
    content_errors: tuple[str, ...]


class ClaimObligation(NamedTuple):
    """README claim mapped to the evidence artifact that must prove it."""
    line_number: int
    claim_text: str
    artifact_path: str
    citation_kind: str
    citation_value: str
    claim_surface: str


class ClaimGatedPhrase(NamedTuple):
    """Claim-like README language that should stay visible to reviewers."""
    line_number: int
    phrase: str
    text: str
    has_inline_citation: bool


def strip_markdown_code(text: str) -> str:
    """Remove Markdown code blocks/spans so examples are not treated as claims."""
    without_fenced_blocks = re.sub(r"(?ms)^```.*?^```", "", text)
    return re.sub(r"`[^`\n]*`", "", without_fenced_blocks)


def strip_markdown_code_preserve_lines(text: str) -> list[str]:
    """Strip Markdown code while preserving line numbers for diagnostics."""
    stripped_lines: list[str] = []
    in_fenced_block = False
    for line in text.splitlines():
        if line.lstrip().startswith("```"):
            in_fenced_block = not in_fenced_block
            stripped_lines.append("")
            continue
        if in_fenced_block:
            stripped_lines.append("")
            continue
        stripped_lines.append(re.sub(r"`[^`\n]*`", "", line))
    return stripped_lines


def is_placeholder_citation(artifact_path: str, correlation_id: str) -> bool:
    """Return true for documentation placeholders, not real evidence claims."""
    return artifact_path.startswith("[") or correlation_id.startswith("[")


def classify_claim_surface(claim_text: str, artifact_path: str) -> str:
    """Classify whether a claim is release-facing or explicitly historical."""
    lowered = f"{claim_text} {artifact_path}".lower()
    if any(
        marker in lowered
        for marker in (
            "historical",
            "snapshot",
            "baseline",
            "planning/",
            "docs/planning/",
            "retained",
            "not treated as current",
        )
    ):
        return "historical_snapshot"
    return "release_facing"


def proof_artifact_family(artifact_path: str) -> str:
    """Return the proof family used for claim obligation diagnostics."""
    normalized = artifact_path.strip().replace("\\", "/")
    for prefix in ("tests/perf/reports/", "docs/evidence/"):
        if normalized.startswith(prefix):
            return prefix.rstrip("/")
    if normalized.startswith("docs/planning/"):
        return "docs/planning"
    return "other"


def parse_citation_obligations(readme_text: str) -> list[ClaimObligation]:
    """Parse README artifact citations with line numbers and claim surface."""
    stripped_lines = strip_markdown_code_preserve_lines(readme_text)
    original_lines = readme_text.splitlines()
    citation_patterns = [
        ("run", re.compile(r'\*\(from ([^,]+), run ([^)]+)\)\*')),
        ("generated", re.compile(r'\*\(from ([^,]+), generated `?([^`)]+)`?\)\*')),
    ]
    obligations: list[ClaimObligation] = []
    seen: set[tuple[int, str, str, str]] = set()
    for line_number, stripped_line in enumerate(stripped_lines, start=1):
        original_line = original_lines[line_number - 1] if line_number - 1 < len(original_lines) else ""
        for citation_kind, citation_pattern in citation_patterns:
            for match in citation_pattern.finditer(stripped_line):
                artifact_path = match.group(1).strip()
                citation_value = match.group(2).strip()
                if is_placeholder_citation(artifact_path, citation_value):
                    continue
                key = (line_number, artifact_path, citation_kind, citation_value)
                if key in seen:
                    continue
                seen.add(key)
                obligations.append(
                    ClaimObligation(
                        line_number=line_number,
                        claim_text=original_line.strip(),
                        artifact_path=artifact_path,
                        citation_kind=citation_kind,
                        citation_value=citation_value,
                        claim_surface=classify_claim_surface(original_line, artifact_path),
                    )
                )
    return obligations


def parse_citations(readme_text: str) -> list[tuple[str, str]]:
    """Parse real README artifact citations, excluding examples and placeholders."""
    return [
        (obligation.artifact_path, obligation.citation_value)
        for obligation in parse_citation_obligations(readme_text)
    ]


CLAIM_GATED_PHRASES = (
    "performance claims",
    "speed claims",
    "benchmark evidence",
    "claim-integrity",
    "release-facing performance",
    "p99 latency",
    "throughput",
    "startup",
    "memory",
    "rss growth",
    "mib",
)


def parse_claim_gated_phrases(readme_text: str) -> list[ClaimGatedPhrase]:
    """Extract claim-gated performance language for proof-obligation reporting."""
    stripped_lines = strip_markdown_code_preserve_lines(readme_text)
    original_lines = readme_text.splitlines()
    phrases: list[ClaimGatedPhrase] = []
    for line_number, stripped_line in enumerate(stripped_lines, start=1):
        lowered = stripped_line.lower()
        for phrase in CLAIM_GATED_PHRASES:
            if phrase not in lowered:
                continue
            original_line = original_lines[line_number - 1] if line_number - 1 < len(original_lines) else ""
            phrases.append(
                ClaimGatedPhrase(
                    line_number=line_number,
                    phrase=phrase,
                    text=original_line.strip(),
                    has_inline_citation="*(from " in original_line,
                )
            )
            break
    return phrases


def as_utc(value: datetime) -> datetime:
    """Normalize datetimes so age checks never mix naive and aware values."""
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def parse_iso_datetime(raw: object) -> datetime | None:
    """Parse RFC3339-ish timestamps, including Rust nanosecond precision."""
    if not isinstance(raw, str):
        return None
    value = raw.strip()
    if not value:
        return None
    if value.endswith("Z"):
        value = f"{value[:-1]}+00:00"
    match = re.match(r"^(.*T\d{2}:\d{2}:\d{2})\.(\d+)(.*)$", value)
    if match:
        prefix, fraction, suffix = match.groups()
        value = f"{prefix}.{fraction[:6].ljust(6, '0')}{suffix}"
    try:
        return as_utc(datetime.fromisoformat(value))
    except ValueError:
        return None


def read_artifact_text(path: Path) -> tuple[str | None, str | None]:
    """Read an artifact as UTF-8 text for correlation and metadata checks."""
    try:
        return path.read_text(encoding="utf-8"), None
    except UnicodeDecodeError:
        return None, "artifact is not UTF-8 text"
    except Exception as exc:
        return None, f"failed to read artifact: {exc}"


def load_json_object(artifact_path: str, text: str) -> tuple[dict[str, object] | None, str | None]:
    """Load a JSON object when the artifact is JSON; ignore other formats."""
    if not artifact_path.endswith(".json"):
        return None, None
    try:
        payload = json.loads(text)
    except json.JSONDecodeError as exc:
        return None, f"artifact JSON failed to parse: {exc}"
    if not isinstance(payload, dict):
        return None, "artifact JSON must be an object"
    return payload, None


def performance_source_binding_error(repo_root: Path, source_commit: str) -> str | None:
    """Require a real source ancestor with only evidence follow-up changes."""
    def git_output(*args: str) -> str | None:
        try:
            result = subprocess.run(
                ["git", "-C", str(repo_root), *args],
                check=False,
                capture_output=True,
                text=True,
                timeout=10,
            )
        except (OSError, subprocess.SubprocessError):
            return None
        return result.stdout if result.returncode == 0 else None

    head = git_output("rev-parse", "--verify", "HEAD^{commit}")
    resolved = git_output("rev-parse", "--verify", f"{source_commit}^{{commit}}")
    if head is None or resolved is None:
        return "budget summary source_commit could not be resolved in this repository"
    head = head.strip()
    if resolved.strip() != source_commit:
        return "budget summary source_commit does not resolve to the exact recorded commit"

    try:
        ancestor = subprocess.run(
            ["git", "-C", str(repo_root), "merge-base", "--is-ancestor", source_commit, head],
            check=False,
            capture_output=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return "budget summary source_commit ancestry could not be verified"
    if ancestor.returncode != 0:
        return "budget summary source_commit is not an ancestor of release HEAD"
    if source_commit == head:
        return None

    changed = git_output("diff", "--name-only", "-z", "--no-renames", source_commit, head)
    if changed is None:
        return "budget summary source-to-release changes could not be inspected"
    changed_paths = tuple(path for path in changed.split("\0") if path)
    if not changed_paths:
        return "budget summary source_commit differs from HEAD without a source diff"
    allowed_prefixes = (
        "tests/perf/reports/",
        "tests/e2e_results/",
        "tests/ext_conformance/reports/",
        "tests/certification/",
        "docs/evidence/",
    )
    unexpected = tuple(
        path for path in changed_paths if not path.startswith(allowed_prefixes)
    )
    if unexpected:
        return f"non-evidence paths changed after budget summary source_commit: {unexpected[:3]!r}"
    return None


def performance_budget_claim_errors(
    payload: dict[str, object],
    repo_root: Path,
    now: datetime,
) -> tuple[str, ...]:
    """Return fail-closed errors for a release-facing performance summary."""
    errors: list[str] = []
    schema = payload.get("schema")
    if schema != PERF_BUDGET_SUMMARY_SCHEMA:
        return (
            f"budget summary schema must be {PERF_BUDGET_SUMMARY_SCHEMA!r}, found {schema!r}",
        )

    claim_readiness = payload.get("claim_readiness")
    if not isinstance(claim_readiness, dict):
        return ("budget summary missing claim_readiness object",)

    status = claim_readiness.get("status")
    authorized = claim_readiness.get("performance_claims_authorized")
    blockers = claim_readiness.get("blocking_reason_codes")
    if status != "claim_ready":
        errors.append(f"budget summary claim_readiness.status={status!r}")
    if authorized is not True:
        errors.append(
            "budget summary claim_readiness.performance_claims_authorized="
            f"{authorized!r}"
        )
    if not isinstance(blockers, list):
        errors.append("budget summary claim_readiness.blocking_reason_codes must be an array")
    elif blockers:
        errors.append(
            f"budget summary claim_readiness.blocking_reason_codes={blockers!r}"
        )

    generated_at_raw = payload.get("generated_at")
    canonical_timestamp = (
        isinstance(generated_at_raw, str)
        and re.fullmatch(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z", generated_at_raw)
        is not None
    )
    generated_at = parse_iso_datetime(generated_at_raw) if canonical_timestamp else None
    if generated_at is None:
        errors.append(
            "budget summary generated_at must use canonical millisecond-precision UTC RFC3339"
        )
    elif generated_at > now + timedelta(minutes=5):
        errors.append("budget summary generated_at is more than five minutes in the future")

    if payload.get("strict_mode") is not True:
        errors.append(f"budget summary strict_mode={payload.get('strict_mode')!r}")
    for field in ("ci_fail", "ci_no_data", "data_contract_failures_count"):
        value = payload.get(field)
        if isinstance(value, bool) or not isinstance(value, int) or value != 0:
            errors.append(f"budget summary has {field}={value!r}")

    total_budgets = payload.get("total_budgets")
    pass_count = payload.get("pass")
    fail_count = payload.get("fail")
    no_data_count = payload.get("no_data")
    if (
        isinstance(total_budgets, bool)
        or not isinstance(total_budgets, int)
        or total_budgets <= 0
        or isinstance(pass_count, bool)
        or not isinstance(pass_count, int)
        or pass_count != total_budgets
        or isinstance(fail_count, bool)
        or not isinstance(fail_count, int)
        or fail_count != 0
        or isinstance(no_data_count, bool)
        or not isinstance(no_data_count, int)
        or no_data_count != 0
    ):
        errors.append(
            "all declared performance budgets must have data and pass: "
            f"pass={pass_count!r}, fail={fail_count!r}, "
            f"no_data={no_data_count!r}, total_budgets={total_budgets!r}"
        )

    ci_enforced = payload.get("ci_enforced")
    ci_with_data = payload.get("ci_with_data")
    if (
        isinstance(ci_enforced, bool)
        or not isinstance(ci_enforced, int)
        or ci_enforced <= 0
        or isinstance(ci_with_data, bool)
        or not isinstance(ci_with_data, int)
        or ci_with_data != ci_enforced
        or (
            isinstance(total_budgets, int)
            and not isinstance(total_budgets, bool)
            and ci_enforced > total_budgets
        )
    ):
        errors.append(
            "budget summary CI data coverage is invalid or incomplete: "
            f"ci_with_data={ci_with_data!r}, ci_enforced={ci_enforced!r}"
        )

    source_commit = payload.get("source_commit")
    if (
        not isinstance(source_commit, str)
        or re.fullmatch(r"[0-9a-f]{40}", source_commit) is None
        or set(source_commit) == {"0"}
    ):
        errors.append("budget summary source_commit is not a full clean-source Git SHA")
    elif (binding_error := performance_source_binding_error(repo_root, source_commit)) is not None:
        errors.append(binding_error)
    run_id = payload.get("run_id")
    correlation_id = payload.get("correlation_id")
    if (
        not isinstance(run_id, str)
        or not run_id.strip()
        or correlation_id != run_id
    ):
        errors.append(
            "budget summary run_id/correlation_id must be identical non-empty strings"
        )

    return tuple(errors)


def check_artifact_content(
    repo_root: Path,
    artifact_path: str,
    correlation_id: str,
    full_path: Path,
    now: datetime,
    staleness_threshold: timedelta,
    claim_surface: str,
) -> tuple[str, ...]:
    """Validate that a cited artifact actually supports the README claim."""
    errors: list[str] = []
    text, read_error = read_artifact_text(full_path)
    if read_error is not None:
        return (read_error,)
    assert text is not None

    if correlation_id not in text:
        errors.append(f"cited run {correlation_id!r} not found in artifact content")

    payload, json_error = load_json_object(artifact_path, text)
    if json_error is not None:
        errors.append(json_error)
        return tuple(errors)
    if payload is None:
        return tuple(errors)

    family = proof_artifact_family(artifact_path)
    if claim_surface == "release_facing" and family not in {
        "tests/perf/reports",
        "docs/evidence",
    }:
        errors.append(
            "release-facing proof obligation must cite tests/perf/reports or docs/evidence "
            f"artifact, got {family}"
        )

    generated_at = parse_iso_datetime(payload.get("generated_at"))
    if generated_at is None:
        errors.append("JSON artifact missing parseable generated_at timestamp")
    elif claim_surface != "historical_snapshot" and now - generated_at > staleness_threshold:
        days_old = (now - generated_at).total_seconds() / 86400
        errors.append(f"artifact generated_at is stale: {days_old:.1f} days old")

    if (
        artifact_path == "tests/perf/reports/budget_summary.json"
        and claim_surface == "release_facing"
    ):
        errors.extend(performance_budget_claim_errors(payload, repo_root, now))

    return tuple(errors)


def check_readme(repo_root: Path, now: datetime | None = None) -> int:
    """Check the README under repo_root for missing or stale artifact citations."""
    readme_path = repo_root / "README.md"
    if not readme_path.exists():
        print(f"ERROR: README.md not found at {readme_path}")
        return 2

    try:
        readme_text = readme_path.read_text(encoding="utf-8")
    except Exception as e:
        print(f"ERROR: Failed to read README.md: {e}")
        return 2

    obligations = parse_citation_obligations(readme_text)
    claim_gated_phrases = parse_claim_gated_phrases(readme_text)

    if not obligations:
        print("INFO: No artifact citations found in README.md")
        if claim_gated_phrases:
            print(f"INFO: Found {len(claim_gated_phrases)} claim-gated phrase(s) without hard citations")
        return 0

    print(f"INFO: Checking {len(obligations)} README proof obligation(s) for freshness...")
    if claim_gated_phrases:
        cited_phrase_count = sum(1 for phrase in claim_gated_phrases if phrase.has_inline_citation)
        print(
            "INFO: Extracted "
            f"{len(claim_gated_phrases)} claim-gated phrase(s) "
            f"({cited_phrase_count} with inline citations)"
        )

    # Check each citation
    stale_count = 0
    missing_count = 0
    results: list[CitationCheck] = []

    # 14-day staleness threshold
    staleness_threshold = timedelta(days=14)
    now = as_utc(now or datetime.now(timezone.utc))
    content_error_count = 0

    for obligation in obligations:
        artifact_path = obligation.artifact_path
        correlation_id = obligation.citation_value
        # Resolve artifact path relative to repo root
        full_path = repo_root / artifact_path

        if not full_path.exists():
            print(
                f"WARNING: line {obligation.line_number}: cited artifact does not exist: {artifact_path}"
            )
            print(
                "  Remediation: regenerate the artifact at the cited path or soften/remove "
                f"the README claim on line {obligation.line_number}."
            )
            missing_count += 1
            results.append(CitationCheck(
                artifact_path=artifact_path,
                correlation_id=correlation_id,
                line_number=obligation.line_number,
                claim_surface=obligation.claim_surface,
                file_exists=False,
                file_mtime=None,
                days_old=None,
                is_stale=False,
                content_errors=(),
            ))
            continue

        try:
            # Get file modification time
            mtime = datetime.fromtimestamp(full_path.stat().st_mtime, timezone.utc)
            age = now - mtime
            days_old = age.total_seconds() / 86400  # Convert to days
            is_stale = obligation.claim_surface != "historical_snapshot" and age > staleness_threshold

            if is_stale:
                print(
                    f"STALE: line {obligation.line_number}: {artifact_path} "
                    f"(age: {days_old:.1f} days, limit: 14 days)"
                )
                print(
                    "  Remediation: regenerate fresh evidence and update the README citation "
                    f"run/provenance on line {obligation.line_number}."
                )
                stale_count += 1
            else:
                freshness_label = "HISTORICAL" if obligation.claim_surface == "historical_snapshot" else "FRESH"
                print(
                    f"{freshness_label}: line {obligation.line_number}: {artifact_path} "
                    f"(age: {days_old:.1f} days, surface={obligation.claim_surface}, "
                    f"proof={proof_artifact_family(artifact_path)})"
                )

            content_errors = check_artifact_content(
                repo_root,
                artifact_path,
                correlation_id,
                full_path,
                now,
                staleness_threshold,
                obligation.claim_surface,
            )
            if content_errors:
                content_error_count += len(content_errors)
                for error in content_errors:
                    print(f"INVALID: line {obligation.line_number}: {artifact_path}: {error}")
                    print(
                        "  Remediation: cite an artifact whose schema/run provenance matches "
                        f"the README claim on line {obligation.line_number}, or remove the claim."
                    )

            results.append(CitationCheck(
                artifact_path=artifact_path,
                correlation_id=correlation_id,
                line_number=obligation.line_number,
                claim_surface=obligation.claim_surface,
                file_exists=True,
                file_mtime=mtime,
                days_old=days_old,
                is_stale=is_stale,
                content_errors=content_errors,
            ))

        except Exception as e:
            print(f"ERROR: Failed to check {artifact_path}: {e}")
            return 2

    # Summary
    print(f"\nSUMMARY:")
    print(f"  Total proof obligations: {len(obligations)}")
    print(f"  Claim-gated phrases extracted: {len(claim_gated_phrases)}")
    print(f"  Fresh artifacts: {len([r for r in results if r.file_exists and not r.is_stale])}")
    print(f"  Stale artifacts: {stale_count}")
    print(f"  Missing artifacts: {missing_count}")
    print(f"  Invalid artifact content checks: {content_error_count}")

    if stale_count > 0:
        print(f"\nFAIL: {stale_count} cited artifact(s) are >14 days stale.")
        print("Evidence claims in README must be backed by fresh artifacts.")
        print("Re-run evidence generation and update citations to resolve this.")
        return 1

    if missing_count > 0:
        print(f"\nFAIL: {missing_count} cited artifact(s) are missing.")
        print("Evidence claims in README must reference checked-in artifacts.")
        return 1

    if content_error_count > 0:
        print(f"\nFAIL: {content_error_count} cited artifact content check(s) failed.")
        print("Evidence claims must cite artifacts with matching run provenance and clean data.")
        return 1

    print("\nPASS: All cited artifacts are fresh and content-valid.")
    return 0


def run_self_test() -> int:
    """Run a small fixture test for examples, placeholders, freshness, and missing files."""
    with TemporaryDirectory() as temp_dir:
        repo_root = Path(temp_dir)
        artifact = repo_root / "tests/perf/reports/fresh.json"
        artifact.parent.mkdir(parents=True)
        now = datetime(2026, 5, 1, 12, 0, 0, tzinfo=timezone.utc)
        artifact.write_text(
            json.dumps(
                {
                    "generated_at": now.isoformat(timespec="milliseconds").replace("+00:00", "Z"),
                    "correlation_id": "fixture-run",
                    "ok": True,
                }
            )
            + "\n",
            encoding="utf-8",
        )
        fresh_ts = now.timestamp()
        os.utime(artifact, (fresh_ts, fresh_ts))

        readme = repo_root / "README.md"
        readme.write_text(
            "\n".join([
                "Example: `*(from [artifact-path], run [correlation-id])*`",
                "```",
                "*(from missing-in-code-block.json, run example)*",
                "```",
                "Claim: *(from tests/perf/reports/fresh.json, run fixture-run)*",
                "",
            ]),
            encoding="utf-8",
        )

        first_output = io.StringIO()
        with contextlib.redirect_stdout(first_output):
            first_result = check_readme(repo_root, now=now)
        first_text = first_output.getvalue()
        if first_result != 0:
            print(first_text)
            print("SELF-TEST FAIL: fresh real citation should pass")
            return 2
        if "[artifact-path]" in first_text or "missing-in-code-block" in first_text:
            print(first_text)
            print("SELF-TEST FAIL: examples/placeholders must not be parsed as claims")
            return 2
        obligations = parse_citation_obligations(readme.read_text(encoding="utf-8"))
        if len(obligations) != 1 or obligations[0].line_number != 5:
            print(obligations)
            print("SELF-TEST FAIL: citation obligations must retain README line numbers")
            return 2
        phrases = parse_claim_gated_phrases(
            "p99 latency `example code` should stay visible when it is claim language\n"
        )
        if len(phrases) != 1 or phrases[0].phrase != "p99 latency":
            print(phrases)
            print("SELF-TEST FAIL: claim-gated performance phrases should be extracted")
            return 2

        readme.write_text(
            readme.read_text(encoding="utf-8")
            + "Broken claim: *(from tests/perf/reports/missing.json, run fixture-run)*\n",
            encoding="utf-8",
        )
        second_output = io.StringIO()
        with contextlib.redirect_stdout(second_output):
            second_result = check_readme(repo_root, now=now)
        if second_result != 1:
            print(second_output.getvalue())
            print("SELF-TEST FAIL: missing real citation should fail")
            return 2

        budget_summary = repo_root / "tests/perf/reports/budget_summary.json"
        budget_summary.write_text(
            json.dumps(
                {
                    "schema": PERF_BUDGET_SUMMARY_SCHEMA,
                    "generated_at": now.isoformat(timespec="milliseconds").replace("+00:00", "Z"),
                    "source_commit": "0123456789abcdef0123456789abcdef01234567",
                    "run_id": "budget-run",
                    "correlation_id": "budget-run",
                    "strict_mode": True,
                    "total_budgets": 1,
                    "pass": 0,
                    "fail": 0,
                    "no_data": 1,
                    "ci_enforced": 1,
                    "ci_with_data": 0,
                    "ci_no_data": 1,
                    "ci_fail": 0,
                    "data_contract_failures_count": 0,
                    "claim_readiness": {
                        "status": "blocked",
                        "performance_claims_authorized": False,
                        "blocking_reason_codes": ["ci_budget_data_missing"],
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(budget_summary, (fresh_ts, fresh_ts))
        readme.write_text(
            "Claim: *(from tests/perf/reports/budget_summary.json, run budget-run)*\n",
            encoding="utf-8",
        )
        third_output = io.StringIO()
        with contextlib.redirect_stdout(third_output):
            third_result = check_readme(repo_root, now=now)
        if third_result != 1 or "ci_no_data=1" not in third_output.getvalue():
            print(third_output.getvalue())
            print("SELF-TEST FAIL: no-data budget summary citation should fail")
            return 2

        legacy_payload = {
            "schema": "pi.perf.budget_summary.v1",
            "generated_at": now.isoformat(timespec="milliseconds").replace("+00:00", "Z"),
            "source_commit": "0123456789abcdef0123456789abcdef01234567",
            "run_id": "budget-run",
            "correlation_id": "budget-run",
            "strict_mode": True,
            "total_budgets": 1,
            "pass": 1,
            "fail": 0,
            "no_data": 0,
            "ci_enforced": 1,
            "ci_with_data": 1,
            "ci_no_data": 0,
            "ci_fail": 0,
            "data_contract_failures_count": 0,
            "claim_readiness": {
                "status": "claim_ready",
                "performance_claims_authorized": True,
                "blocking_reason_codes": [],
            },
        }
        legacy_errors = performance_budget_claim_errors(legacy_payload, repo_root, now)
        if not any(PERF_BUDGET_SUMMARY_SCHEMA in error for error in legacy_errors):
            print(legacy_errors)
            print("SELF-TEST FAIL: legacy v1 budget summary must fail closed")
            return 2

        future_payload = dict(legacy_payload)
        future_payload["schema"] = PERF_BUDGET_SUMMARY_SCHEMA
        future_payload["generated_at"] = (
            now + timedelta(minutes=6)
        ).isoformat(timespec="milliseconds").replace("+00:00", "Z")
        future_errors = performance_budget_claim_errors(future_payload, repo_root, now)
        if not any("more than five minutes in the future" in error for error in future_errors):
            print(future_errors)
            print("SELF-TEST FAIL: future-dated v2 budget summary must fail closed")
            return 2
        if not any("could not be resolved" in error for error in future_errors):
            print(future_errors)
            print("SELF-TEST FAIL: forged v2 source_commit must fail closed")
            return 2

        provenance_mismatch = repo_root / "tests/perf/reports/provenance_mismatch.json"
        provenance_mismatch.write_text(
            json.dumps(
                {
                    "generated_at": now.isoformat(),
                    "correlation_id": "actual-run",
                    "ok": True,
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(provenance_mismatch, (fresh_ts, fresh_ts))
        readme.write_text(
            "Claim: *(from tests/perf/reports/provenance_mismatch.json, run cited-run)*\n",
            encoding="utf-8",
        )
        fourth_output = io.StringIO()
        with contextlib.redirect_stdout(fourth_output):
            fourth_result = check_readme(repo_root, now=now)
        fourth_text = fourth_output.getvalue()
        if fourth_result != 1 or "cited run 'cited-run' not found" not in fourth_text:
            print(fourth_text)
            print("SELF-TEST FAIL: cited run mismatch should fail")
            return 2

        generated_stale = repo_root / "tests/perf/reports/generated_stale.json"
        generated_stale.write_text(
            json.dumps(
                {
                    "generated_at": (now - timedelta(days=30)).isoformat(),
                    "correlation_id": "stale-run",
                    "ok": True,
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(generated_stale, (fresh_ts, fresh_ts))
        readme.write_text(
            "Claim: *(from tests/perf/reports/generated_stale.json, run stale-run)*\n",
            encoding="utf-8",
        )
        fifth_output = io.StringIO()
        with contextlib.redirect_stdout(fifth_output):
            fifth_result = check_readme(repo_root, now=now)
        fifth_text = fifth_output.getvalue()
        if fifth_result != 1 or "artifact generated_at is stale" not in fifth_text:
            print(fifth_text)
            print("SELF-TEST FAIL: stale generated_at should fail even with fresh mtime")
            return 2

        historical_snapshot = repo_root / "docs/planning/historical_snapshot.json"
        historical_snapshot.parent.mkdir(parents=True, exist_ok=True)
        historical_snapshot.write_text(
            json.dumps(
                {
                    "generated_at": (now - timedelta(days=45)).isoformat(),
                    "correlation_id": "historical-run",
                    "ok": True,
                }
            )
            + "\n",
            encoding="utf-8",
        )
        old_ts = (now - timedelta(days=45)).timestamp()
        os.utime(historical_snapshot, (old_ts, old_ts))
        readme.write_text(
            "Historical benchmark snapshot: *(from docs/planning/historical_snapshot.json, run historical-run)*\n",
            encoding="utf-8",
        )
        historical_output = io.StringIO()
        with contextlib.redirect_stdout(historical_output):
            historical_result = check_readme(repo_root, now=now)
        historical_text = historical_output.getvalue()
        if historical_result != 0 or "surface=historical_snapshot" not in historical_text:
            print(historical_text)
            print("SELF-TEST FAIL: historical snapshots should be mapped but not freshness-blocking")
            return 2

    print("SELF-TEST PASS")
    return 0


def main() -> int:
    """Main entry point."""
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run fixture-based checks for citation parsing behavior",
    )
    args = parser.parse_args()
    if args.self_test:
        return run_self_test()
    repo_root = Path(__file__).resolve().parent.parent
    return check_readme(repo_root)


if __name__ == "__main__":
    sys.exit(main())
