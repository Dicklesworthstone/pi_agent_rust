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
import fnmatch
import hashlib
import io
import json
import math
import os
import re
import stat
import subprocess
import sys
import tomllib
from datetime import datetime, timedelta, timezone
from pathlib import Path, PureWindowsPath
from tempfile import TemporaryDirectory
from typing import NamedTuple


PERF_BUDGET_SUMMARY_SCHEMA = "pi.perf.budget_summary.v2"
PERF_BUDGET_INVENTORY_SHA256 = (
    "96e3147ef23e1c634d56265581975a2b619ac9a701f4839ef6f3f4b3987226ad"
)
PERF_SUMMARY_FIELDS = frozenset(
    {
        "schema",
        "generated_at",
        "source_commit",
        "run_id",
        "correlation_id",
        "strict_mode",
        "total_budgets",
        "ci_enforced",
        "ci_with_data",
        "ci_fail",
        "ci_no_data",
        "pass",
        "fail",
        "no_data",
        "data_contract_failures_count",
        "failing_data_contracts",
        "budgets",
        "budget_results",
        "claim_readiness",
    }
)
PERF_BUDGET_FIELDS = frozenset(
    {
        "name",
        "category",
        "metric",
        "unit",
        "threshold",
        "comparison",
        "methodology",
        "ci_enforced",
    }
)
PERF_RESULT_REQUIRED_FIELDS = frozenset(
    {
        "budget_name",
        "category",
        "threshold",
        "comparison",
        "unit",
        "actual",
        "status",
        "source",
        "ci_enforced",
    }
)
PERF_RESULT_OPTIONAL_FIELDS = frozenset({"failure_reason"})
PERF_FAILURE_REQUIRED_FIELDS = frozenset({"contract_id", "detail", "remediation"})
PERF_FAILURE_OPTIONAL_FIELDS = frozenset({"budget_name"})
PERF_CLAIM_READINESS_FIELDS = frozenset(
    {"status", "performance_claims_authorized", "blocking_reason_codes"}
)
PERF_LINEAGE_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:/-]{0,255}")
PERF_OBJECT_ID_RE = re.compile(r"(?:[0-9a-f]{40}|[0-9a-f]{64})")
PERF_TIMESTAMP_RE = re.compile(
    r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z"
)
CANONICAL_PERFORMANCE_SUMMARY_PATH = "tests/perf/reports/budget_summary.json"
QUANTITATIVE_PERFORMANCE_METRIC_RE = re.compile(
    r"\b(?:startup|cold[- ]?start|latency|p(?:50|90|95|99|999)|throughput|"
    r"memory|rss|binary[ -]size)\b",
    re.IGNORECASE,
)
QUANTITATIVE_PERFORMANCE_VALUE_RE = re.compile(
    r"(?<![A-Za-z0-9_.])\d+(?:\.\d+)?(?:[kKmMgG])?\s*(?:"
    r"ns|u?s|[µμ]s|ms|seconds?|bytes?|[kmgt]i?b|"
    r"(?:calls?|requests?|reqs?|ops?|tokens?)\s*(?:/|per\s+)(?:s|sec|second)s?"
    r")\b",
    re.IGNORECASE,
)
QUANTITATIVE_PERFORMANCE_RELATIVE_RE = re.compile(
    r"(?:\b\d+(?:\.\d+)?\s*[x×]\s*(?:as\s+)?(?:fast(?:er)?|speedup|throughput)\b|"
    r"\b\d+(?:\.\d+)?\s*%\s*(?:faster|slower|lower|higher|less|more)\b)",
    re.IGNORECASE,
)
QUANTITATIVE_PERFORMANCE_COMPARISON_RE = re.compile(
    r"(?:\b(?:under|below|over|above|within|at\s+most|at\s+least|less\s+than|"
    r"more\s+than)\b|[<>]=?)",
    re.IGNORECASE,
)
QUANTITATIVE_PERFORMANCE_POLICY_RE = re.compile(
    r"\b(?:budget(?:ed)?|threshold|target(?:ed)?|limit|pending|not\s+measured|"
    r"no\s+measurement|not\s+authorized|required\s+before|requires?\b[^.]{0,40}\bbefore)\b",
    re.IGNORECASE,
)
QUANTITATIVE_PERFORMANCE_OBSERVED_RE = re.compile(
    r"\b(?:actual|achieved|measured|observed|reported|averag(?:e|ed|es)|uses?|consumes?)\b",
    re.IGNORECASE,
)
MAX_FUTURE_CLOCK_SKEW = timedelta(minutes=5)


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


class QuantitativePerformanceClaim(NamedTuple):
    """A numeric performance assertion and its canonical citation state."""
    line_number: int
    text: str
    has_canonical_inline_citation: bool


class GitRepositoryBinding(NamedTuple):
    """Filesystem-derived Git identity pinned to the inspected worktree."""
    worktree: Path
    git_dir: Path


class ReleaseArtifactSnapshot(NamedTuple):
    """One immutable HEAD-bound byte snapshot used for release interpretation."""
    artifact_path: str
    full_path: Path
    repository: GitRepositoryBinding
    head: str
    head_mode: bytes
    contents: bytes
    device: int
    inode: int
    size: int
    mtime_ns: int
    file_mtime: datetime


def strip_markdown_code(text: str) -> str:
    """Remove Markdown code blocks/spans so examples are not treated as claims."""
    without_fenced_blocks = re.sub(r"(?ms)^```.*?^```", "", text)
    without_fenced_blocks = re.sub(
        r"(\*\(from [^,\n]+, generated )`([^`\n]+)`(\)\*)",
        r"\1\2\3",
        without_fenced_blocks,
    )
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
        citation_safe_line = re.sub(
            r"(\*\(from [^,\n]+, generated )`([^`\n]+)`(\)\*)",
            r"\1\2\3",
            line,
        )
        stripped_lines.append(re.sub(r"`[^`\n]*`", "", citation_safe_line))
    return stripped_lines


def is_placeholder_citation(artifact_path: str, correlation_id: str) -> bool:
    """Return true for documentation placeholders, not real evidence claims."""
    return artifact_path.startswith("[") or correlation_id.startswith("[")


def classify_claim_surface(claim_text: str, artifact_path: str) -> str:
    """Classify whether a claim is release-facing or explicitly historical."""
    # Do not let traversal text inside a citation manufacture a historical
    # classification. The caller supplies the canonical repository-relative
    # path after containment checks; only prose outside citations and that
    # canonical path participate in classification.
    prose = re.sub(r"\*\(from [^)]+\)\*", "", claim_text)
    normalized_prose = " ".join(prose.casefold().split())
    # Historical evidence is an explicit semantic contract, not a keyword
    # heuristic.  Generic words such as "baseline", "snapshot", or "retained"
    # routinely appear in current comparative claims and must never disable
    # freshness or strict performance-proof validation.  Historical-only
    # citations live under docs/planning and carry the exact, whole-line label
    # below.  Requiring equality (rather than accepting the label as a prefix)
    # prevents a current claim appended to the disclaimer from inheriting the
    # historical exemption.
    if (
        artifact_path.startswith("docs/planning/")
        and normalized_prose
        == "historical-only; not a current release claim: benchmark snapshot"
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


def resolve_repo_artifact_path(
    repo_root: Path,
    artifact_path: str,
) -> tuple[str | None, Path | None, str | None]:
    """Resolve a citation to a contained canonical repository-relative path."""
    raw = artifact_path.strip()
    if not raw:
        return None, None, "artifact path must not be empty"
    if "\0" in raw:
        return None, None, "artifact path must not contain NUL"

    normalized = raw.replace("\\", "/")
    if Path(normalized).is_absolute() or PureWindowsPath(raw).is_absolute():
        return None, None, f"artifact path must be repository-relative: {artifact_path!r}"
    path_parts = normalized.split("/")
    if any(part in {"", ".", ".."} for part in path_parts):
        return None, None, f"artifact path must be canonical: {artifact_path!r}"

    try:
        canonical_root = repo_root.resolve(strict=True)
        candidate = canonical_root.joinpath(*path_parts)
        current = canonical_root
        for part in path_parts:
            current /= part
            try:
                metadata = current.lstat()
            except FileNotFoundError:
                break
            if stat.S_ISLNK(metadata.st_mode):
                return (
                    None,
                    None,
                    f"artifact path must not contain symlink components: {artifact_path!r}",
                )
        resolved = candidate.resolve(strict=False)
    except FileNotFoundError:
        return None, None, f"repository root does not exist: {repo_root}"
    except (OSError, RuntimeError):
        return None, None, f"artifact path could not be resolved safely: {artifact_path!r}"

    try:
        relative = resolved.relative_to(canonical_root)
    except ValueError:
        return None, None, f"artifact path escapes repository root: {artifact_path!r}"

    if resolved == canonical_root:
        return None, None, "artifact path must identify a file below the repository root"
    return relative.as_posix(), resolved, None


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


def parse_quantitative_performance_claims(
    readme_text: str,
) -> list[QuantitativePerformanceClaim]:
    """Find achieved numeric performance assertions, excluding policy-only prose."""
    stripped_lines = strip_markdown_code_preserve_lines(readme_text)
    original_lines = readme_text.splitlines()
    canonical_citation_lines = {
        obligation.line_number
        for obligation in parse_citation_obligations(readme_text)
        if obligation.artifact_path.strip().replace("\\", "/")
        == CANONICAL_PERFORMANCE_SUMMARY_PATH
    }

    claims: list[QuantitativePerformanceClaim] = []
    for line_number, stripped_line in enumerate(stripped_lines, start=1):
        # An empty stripped line is either fenced code or an inline-code-only
        # example. Neither is user-facing performance prose.
        if not stripped_line.strip():
            continue
        original_line = (
            original_lines[line_number - 1]
            if line_number - 1 < len(original_lines)
            else ""
        )
        prose = re.sub(r"\*\(from [^)]+\)\*", "", original_line)
        # Numeric results are commonly formatted as inline code. Preserve their
        # contents while removing only the Markdown delimiters.
        prose = re.sub(r"`([^`\n]*)`", r"\1", prose)

        relative_claim = QUANTITATIVE_PERFORMANCE_RELATIVE_RE.search(prose) is not None
        metric_claim = (
            QUANTITATIVE_PERFORMANCE_METRIC_RE.search(prose) is not None
            and QUANTITATIVE_PERFORMANCE_VALUE_RE.search(prose) is not None
        )
        if not relative_claim and not metric_claim:
            continue

        comparison_claim = (
            QUANTITATIVE_PERFORMANCE_COMPARISON_RE.search(prose) is not None
        )
        observed_assertion_prose = re.sub(
            r"\b(?:(?:has\s+)?not(?:\s+yet)?(?:\s+been)?\s+measured|"
            r"never(?:\s+been)?\s+measured|no\s+measurement)\b",
            "",
            prose,
            flags=re.IGNORECASE,
        )
        policy_only = (
            QUANTITATIVE_PERFORMANCE_POLICY_RE.search(prose) is not None
            and QUANTITATIVE_PERFORMANCE_OBSERVED_RE.search(observed_assertion_prose)
            is None
            and not comparison_claim
            and not relative_claim
        )
        if policy_only:
            continue

        claims.append(
            QuantitativePerformanceClaim(
                line_number=line_number,
                text=original_line.strip(),
                has_canonical_inline_citation=(
                    line_number in canonical_citation_lines
                ),
            )
        )
    return claims


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


def canonicalize_iso_datetime(raw: object) -> str | None:
    """Canonicalize an ISO timestamp without discarding sub-microsecond digits."""
    if not isinstance(raw, str):
        return None
    value = raw.strip()
    match = re.fullmatch(
        r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.(\d+))?(Z|[+-]\d{2}:\d{2})?",
        value,
    )
    if match is None:
        return None
    second_text, fraction, offset = match.groups()
    try:
        local_second = datetime.fromisoformat(
            second_text + ("+00:00" if offset in (None, "Z") else offset)
        )
    except ValueError:
        return None
    utc_second = local_second.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S")
    normalized_fraction = (fraction or "").rstrip("0")
    suffix = f".{normalized_fraction}" if normalized_fraction else ""
    return f"{utc_second}{suffix}Z"


def decode_artifact_text(contents: bytes) -> tuple[str | None, str | None]:
    """Decode an already captured artifact byte snapshot as UTF-8."""
    try:
        return contents.decode("utf-8", "strict"), None
    except UnicodeDecodeError:
        return None, "artifact is not UTF-8 text"


def load_json_object(artifact_path: str, text: str) -> tuple[dict[str, object] | None, str | None]:
    """Load a JSON object when the artifact is JSON; ignore other formats."""
    if not artifact_path.endswith(".json"):
        return None, None

    def reject_duplicate_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
        value: dict[str, object] = {}
        for key, item in pairs:
            if key in value:
                raise ValueError(f"duplicate JSON object key: {key}")
            value[key] = item
        return value

    try:
        payload = json.loads(text, object_pairs_hook=reject_duplicate_keys)
    except (json.JSONDecodeError, ValueError) as exc:
        return None, f"artifact JSON failed to parse: {exc}"
    if not isinstance(payload, dict):
        return None, "artifact JSON must be an object"
    return payload, None


class PerformanceContractError(ValueError):
    """Raised internally when a performance summary violates its exact schema."""


def _sanitized_git_environment() -> dict[str, str]:
    """Return an environment that cannot redirect repository inspection."""
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("GIT_")
    }
    env.update(
        {
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_LITERAL_PATHSPECS": "1",
            "GIT_NO_REPLACE_OBJECTS": "1",
            "GIT_OPTIONAL_LOCKS": "0",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    return env


def _filesystem_git_repository(
    repo_root: Path,
) -> tuple[GitRepositoryBinding | None, str | None]:
    """Derive worktree/git-dir identity without consulting redirectable Git config."""
    try:
        worktree = repo_root.resolve(strict=True)
        if not worktree.is_dir():
            return None, "repository root must be a directory"
        dot_git = worktree / ".git"
        dot_git_metadata = dot_git.lstat()
        if stat.S_ISLNK(dot_git_metadata.st_mode):
            return None, "repository .git entry must not be a symlink"

        if stat.S_ISDIR(dot_git_metadata.st_mode):
            git_dir = dot_git.resolve(strict=True)
        elif stat.S_ISREG(dot_git_metadata.st_mode):
            gitfile = dot_git.read_text(encoding="utf-8")
            stripped = gitfile.strip()
            if (
                "\0" in gitfile
                or "\n" in stripped
                or "\r" in stripped
                or not stripped.startswith("gitdir:")
            ):
                return None, "repository .git file is not a canonical gitdir pointer"
            raw_git_dir = stripped.removeprefix("gitdir:").strip()
            if not raw_git_dir:
                return None, "repository .git file has an empty gitdir pointer"
            candidate = Path(raw_git_dir)
            if not candidate.is_absolute():
                candidate = worktree / candidate
            if candidate.is_symlink():
                return None, "repository gitdir target must not be a symlink"
            git_dir = candidate.resolve(strict=True)
        else:
            return None, "repository .git entry must be a directory or gitfile"

        git_dir_metadata = git_dir.lstat()
        if stat.S_ISLNK(git_dir_metadata.st_mode) or not stat.S_ISDIR(
            git_dir_metadata.st_mode
        ):
            return None, "repository gitdir must be a real directory"
        head_metadata = (git_dir / "HEAD").lstat()
        if stat.S_ISLNK(head_metadata.st_mode) or not stat.S_ISREG(
            head_metadata.st_mode
        ):
            return None, "repository gitdir HEAD must be a real regular file"
    except UnicodeError:
        return None, "repository .git file must be UTF-8"
    except (FileNotFoundError, OSError, RuntimeError) as exc:
        return None, f"repository Git identity could not be resolved safely: {exc}"
    return GitRepositoryBinding(worktree=worktree, git_dir=git_dir), None


def _git_bytes(
    repository: GitRepositoryBinding,
    *args: str,
) -> tuple[bytes | None, str | None]:
    """Run Git pinned to one filesystem-derived worktree and real git-dir."""
    command = [
        "git",
        "--no-optional-locks",
        f"--git-dir={repository.git_dir}",
        f"--work-tree={repository.worktree}",
        "-c",
        f"core.worktree={repository.worktree}",
        "-c",
        "core.bare=false",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
        "-c",
        f"core.excludesFile={os.devnull}",
        *args,
    ]
    try:
        result = subprocess.run(
            command,
            check=False,
            capture_output=True,
            cwd=repository.worktree,
            timeout=10,
            env=_sanitized_git_environment(),
        )
    except (OSError, subprocess.SubprocessError) as exc:
        return None, f"git {' '.join(args)} failed to run: {exc}"
    if result.returncode != 0:
        diagnostic = result.stderr.decode("utf-8", "replace").strip()
        return None, f"git {' '.join(args)} failed: {diagnostic or f'exit {result.returncode}'}"
    return result.stdout, None


def _canonical_git_object_id(raw: bytes, label: str) -> tuple[str | None, str | None]:
    try:
        value = raw.decode("ascii", "strict").strip()
    except UnicodeError:
        return None, f"{label} is not an ASCII object ID"
    if PERF_OBJECT_ID_RE.fullmatch(value) is None or set(value) == {"0"}:
        return None, f"{label} is not a canonical non-zero Git object ID"
    return value, None


def _repository_identity_error(repository: GitRepositoryBinding) -> str | None:
    top_raw, git_error = _git_bytes(repository, "rev-parse", "--show-toplevel")
    if git_error is not None:
        return f"repository worktree identity could not be verified: {git_error}"
    git_dir_raw, git_error = _git_bytes(repository, "rev-parse", "--absolute-git-dir")
    if git_error is not None:
        return f"repository gitdir identity could not be verified: {git_error}"
    assert top_raw is not None and git_dir_raw is not None
    try:
        reported_top = Path(os.fsdecode(top_raw).strip()).resolve(strict=True)
        reported_git_dir = Path(os.fsdecode(git_dir_raw).strip()).resolve(strict=True)
    except (FileNotFoundError, OSError, RuntimeError):
        return "repository Git identity reported paths that could not be resolved"
    if reported_top != repository.worktree or reported_git_dir != repository.git_dir:
        return (
            "repository Git identity disagrees with its filesystem-derived binding "
            f"(worktree={reported_top}, git_dir={reported_git_dir})"
        )
    return None


def _repository_clean_state_error(repository: GitRepositoryBinding) -> str | None:
    status, git_error = _git_bytes(
        repository,
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=none",
        "--no-renames",
    )
    if git_error is not None:
        return f"budget summary repository cleanliness could not be verified: {git_error}"
    assert status is not None
    if status:
        entries = [
            entry.decode("utf-8", "replace")
            for entry in status.split(b"\0")
            if entry
        ]
        return f"budget summary repository is not clean: {entries[:3]!r}"

    index_listing, git_error = _git_bytes(repository, "ls-files", "-v", "-z")
    if git_error is not None:
        return f"budget summary default-index flags could not be inspected: {git_error}"
    assert index_listing is not None
    flagged_paths = [
        entry[2:].decode("utf-8", "replace")
        for entry in index_listing.split(b"\0")
        if entry and not entry.startswith(b"H ")
    ]
    if flagged_paths:
        return (
            "budget summary repository uses non-default index flags: "
            f"{flagged_paths[:3]!r}"
        )
    return None


def _package_includes(path: str, patterns: object) -> bool:
    if not isinstance(patterns, list):
        raise PerformanceContractError("source Cargo.toml package.include must be an array")
    for raw_pattern in patterns:
        if not isinstance(raw_pattern, str) or not raw_pattern:
            raise PerformanceContractError(
                "source Cargo.toml package.include entries must be non-empty strings"
            )
        pattern = raw_pattern.removeprefix("/")
        if fnmatch.fnmatchcase(path, pattern):
            return True
        if pattern.endswith("/**") and path.startswith(pattern[:-3].rstrip("/") + "/"):
            return True
    return False


def _path_has_symlink_component(repo_root: Path, artifact_path: str) -> bool:
    current = repo_root.resolve(strict=True)
    for part in Path(artifact_path.replace("\\", "/")).parts:
        if part in ("", "."):
            continue
        current = current.parent if part == ".." else current / part
        if current.is_symlink():
            return True
    return False


def _worktree_git_mode(metadata: os.stat_result) -> bytes:
    """Map live executable bits to the only regular-file modes Git records."""
    executable_mask = stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
    return b"100755" if metadata.st_mode & executable_mask else b"100644"


def revalidate_release_artifact_snapshot(
    snapshot: ReleaseArtifactSnapshot,
) -> str | None:
    """Fail if identity, HEAD, metadata, mode, or bytes changed after capture."""
    canonical_path, full_path, path_error = resolve_repo_artifact_path(
        snapshot.repository.worktree, snapshot.artifact_path
    )
    if path_error is not None:
        return f"release-facing artifact changed during validation: {path_error}"
    if canonical_path != snapshot.artifact_path or full_path != snapshot.full_path:
        return "release-facing artifact path identity changed during validation"

    repository, repository_error = _filesystem_git_repository(
        snapshot.repository.worktree
    )
    if repository_error is not None or repository != snapshot.repository:
        return "release-facing artifact repository identity changed during validation"
    identity_error = _repository_identity_error(snapshot.repository)
    if identity_error is not None:
        return identity_error

    final_head_raw, git_error = _git_bytes(
        snapshot.repository, "rev-parse", "--verify", "HEAD^{commit}"
    )
    if git_error is not None:
        return f"release-facing artifact HEAD could not be revalidated: {git_error}"
    assert final_head_raw is not None
    final_head, final_head_error = _canonical_git_object_id(
        final_head_raw, "release-facing artifact final HEAD"
    )
    if final_head_error is not None:
        return final_head_error
    if final_head != snapshot.head:
        return "release-facing artifact HEAD changed during validation"

    try:
        final_metadata = snapshot.full_path.lstat()
        final_bytes = snapshot.full_path.read_bytes()
    except OSError as exc:
        return f"release-facing artifact could not be re-read: {exc}"
    if not stat.S_ISREG(final_metadata.st_mode):
        return "release-facing artifact type changed during validation"
    if _worktree_git_mode(final_metadata) != snapshot.head_mode:
        return "release-facing artifact executable mode changed or differs from HEAD"
    final_identity = (
        final_metadata.st_dev,
        final_metadata.st_ino,
        final_metadata.st_size,
        final_metadata.st_mtime_ns,
    )
    captured_identity = (
        snapshot.device,
        snapshot.inode,
        snapshot.size,
        snapshot.mtime_ns,
    )
    if final_identity != captured_identity or final_bytes != snapshot.contents:
        return "release-facing artifact metadata or raw bytes changed during validation"
    return None


def capture_release_artifact_snapshot(
    repo_root: Path,
    artifact_repo_path: str,
) -> tuple[ReleaseArtifactSnapshot | None, str | None]:
    """Capture the exact regular-file bytes at one immutable release HEAD."""
    canonical_path, full_path, path_error = resolve_repo_artifact_path(
        repo_root, artifact_repo_path
    )
    if path_error is not None:
        return None, path_error
    assert canonical_path is not None and full_path is not None

    try:
        metadata = full_path.lstat()
    except OSError as exc:
        return None, f"release-facing artifact could not be inspected: {exc}"
    if not stat.S_ISREG(metadata.st_mode):
        return None, (
            "release-facing artifact must be a regular file without symlink components"
        )

    repository, repository_error = _filesystem_git_repository(repo_root)
    if repository_error is not None:
        return None, (
            f"release-facing artifact repository identity is unsafe: {repository_error}"
        )
    assert repository is not None
    identity_error = _repository_identity_error(repository)
    if identity_error is not None:
        return None, identity_error

    head_raw, git_error = _git_bytes(
        repository, "rev-parse", "--verify", "HEAD^{commit}"
    )
    if git_error is not None:
        return None, f"release-facing artifact HEAD could not be resolved: {git_error}"
    assert head_raw is not None
    head, head_error = _canonical_git_object_id(
        head_raw, "release-facing artifact HEAD"
    )
    if head_error is not None:
        return None, head_error
    assert head is not None

    tree_entry, git_error = _git_bytes(
        repository, "ls-tree", "-z", "--full-tree", head, "--", canonical_path
    )
    if git_error is not None:
        return None, (
            f"release-facing artifact HEAD entry could not be inspected: {git_error}"
        )
    assert tree_entry is not None
    entries = [entry for entry in tree_entry.split(b"\0") if entry]
    if len(entries) != 1 or b"\t" not in entries[0]:
        return None, "release-facing artifact must be tracked at HEAD"
    raw_metadata, tracked_path = entries[0].split(b"\t", 1)
    metadata_parts = raw_metadata.split()
    if (
        tracked_path != os.fsencode(canonical_path)
        or len(metadata_parts) != 3
        or metadata_parts[1] != b"blob"
        or metadata_parts[0] not in {b"100644", b"100755"}
    ):
        return None, (
            "release-facing artifact HEAD entry must be one regular-file blob"
        )

    blob_oid, blob_error = _canonical_git_object_id(
        metadata_parts[2], "release-facing artifact HEAD blob"
    )
    if blob_error is not None:
        return None, blob_error
    assert blob_oid is not None
    head_bytes, git_error = _git_bytes(repository, "cat-file", "blob", blob_oid)
    if git_error is not None:
        return None, f"release-facing artifact HEAD bytes could not be read: {git_error}"
    assert head_bytes is not None
    try:
        metadata = full_path.lstat()
        current_bytes = full_path.read_bytes()
    except OSError as exc:
        return None, f"release-facing artifact worktree bytes could not be read: {exc}"
    if not stat.S_ISREG(metadata.st_mode):
        return None, "release-facing artifact type changed during capture"
    if _worktree_git_mode(metadata) != metadata_parts[0]:
        return None, "release-facing artifact executable mode does not exactly match HEAD"
    if current_bytes != head_bytes:
        return None, "release-facing artifact worktree bytes do not exactly match HEAD"

    snapshot = ReleaseArtifactSnapshot(
        artifact_path=canonical_path,
        full_path=full_path,
        repository=repository,
        head=head,
        head_mode=metadata_parts[0],
        contents=head_bytes,
        device=metadata.st_dev,
        inode=metadata.st_ino,
        size=metadata.st_size,
        mtime_ns=metadata.st_mtime_ns,
        file_mtime=datetime.fromtimestamp(metadata.st_mtime, timezone.utc),
    )
    revalidation_error = revalidate_release_artifact_snapshot(snapshot)
    if revalidation_error is not None:
        return None, revalidation_error
    return snapshot, None


def release_artifact_head_commit_time(
    snapshot: ReleaseArtifactSnapshot,
) -> tuple[datetime | None, str | None]:
    """Return the HEAD-bound time of the last commit touching one artifact path."""
    raw_timestamp, git_error = _git_bytes(
        snapshot.repository,
        "log",
        "-1",
        "--format=%ct",
        snapshot.head,
        "--",
        snapshot.artifact_path,
    )
    if git_error is not None:
        return None, (
            "release-facing non-JSON artifact commit time could not be resolved: "
            f"{git_error}"
        )
    assert raw_timestamp is not None
    if re.fullmatch(rb"[0-9]{1,20}\n", raw_timestamp) is None:
        return None, (
            "release-facing non-JSON artifact has no canonical HEAD-bound commit time"
        )
    timestamp = int(raw_timestamp)
    if timestamp > 2**63 - 1:
        return None, (
            "release-facing non-JSON artifact commit time exceeds the signed 64-bit range"
        )
    try:
        return datetime.fromtimestamp(timestamp, timezone.utc), None
    except (OverflowError, OSError, ValueError):
        return None, (
            "release-facing non-JSON artifact commit time is outside the supported range"
        )


def release_artifact_head_binding_error(
    repo_root: Path,
    artifact_repo_path: str,
) -> str | None:
    """Compatibility wrapper for callers that only need a binding verdict."""
    snapshot, capture_error = capture_release_artifact_snapshot(
        repo_root, artifact_repo_path
    )
    if capture_error is not None:
        return capture_error
    assert snapshot is not None
    final_error = revalidate_release_artifact_snapshot(snapshot)
    if final_error is not None:
        return final_error
    return None


def performance_source_binding_error(
    repo_root: Path,
    source_commit: str,
    artifact_repo_path: str,
) -> str | None:
    """Bind structural evidence to one clean checkout and immutable release HEAD."""
    canonical_path, full_path, path_error = resolve_repo_artifact_path(
        repo_root, artifact_repo_path
    )
    if path_error is not None:
        return path_error
    assert canonical_path is not None and full_path is not None

    try:
        if _path_has_symlink_component(repo_root, artifact_repo_path):
            return "performance summary path must not contain symlink components"
    except (FileNotFoundError, OSError, RuntimeError):
        return "performance summary path symlink status could not be verified"
    if not full_path.is_file():
        return "performance summary must be a regular file"

    repository, repository_error = _filesystem_git_repository(repo_root)
    if repository_error is not None:
        return f"budget summary repository identity is unsafe: {repository_error}"
    assert repository is not None
    identity_error = _repository_identity_error(repository)
    if identity_error is not None:
        return identity_error

    head_raw, git_error = _git_bytes(
        repository, "rev-parse", "--verify", "HEAD^{commit}"
    )
    if git_error is not None:
        return f"budget summary release HEAD could not be resolved: {git_error}"
    assert head_raw is not None
    head, head_error = _canonical_git_object_id(
        head_raw, "budget summary release HEAD"
    )
    if head_error is not None:
        return head_error
    assert head is not None

    clean_error = _repository_clean_state_error(repository)
    if clean_error is not None:
        return clean_error

    tree_entry, git_error = _git_bytes(
        repository, "ls-tree", "-z", "--full-tree", head, "--", canonical_path
    )
    if git_error is not None:
        return f"budget summary HEAD tree entry could not be inspected: {git_error}"
    assert tree_entry is not None
    entries = [entry for entry in tree_entry.split(b"\0") if entry]
    if len(entries) != 1 or b"\t" not in entries[0]:
        return "performance summary is not tracked at HEAD"
    metadata, tracked_path = entries[0].split(b"\t", 1)
    metadata_parts = metadata.split()
    if (
        tracked_path != os.fsencode(canonical_path)
        or len(metadata_parts) != 3
        or metadata_parts[1] != b"blob"
        or metadata_parts[0] not in {b"100644", b"100755"}
    ):
        return "performance summary HEAD entry must be a tracked regular-file blob"

    blob_oid, blob_error = _canonical_git_object_id(
        metadata_parts[2], "performance summary HEAD blob"
    )
    if blob_error is not None:
        return blob_error
    assert blob_oid is not None
    head_bytes, git_error = _git_bytes(repository, "cat-file", "blob", blob_oid)
    if git_error is not None:
        return f"budget summary bytes at HEAD could not be read: {git_error}"
    assert head_bytes is not None
    try:
        current_metadata = full_path.lstat()
        current_bytes = full_path.read_bytes()
    except OSError as exc:
        return f"budget summary current bytes could not be read: {exc}"
    if (
        not stat.S_ISREG(current_metadata.st_mode)
        or _worktree_git_mode(current_metadata) != metadata_parts[0]
    ):
        return "performance summary current executable mode does not exactly match HEAD"
    if current_bytes != head_bytes:
        return "performance summary current bytes do not exactly match HEAD"

    resolved_raw, git_error = _git_bytes(
        repository, "rev-parse", "--verify", f"{source_commit}^{{commit}}"
    )
    if git_error is not None:
        return f"budget summary source_commit could not be resolved: {git_error}"
    assert resolved_raw is not None
    resolved, resolved_error = _canonical_git_object_id(
        resolved_raw, "budget summary source_commit resolution"
    )
    if resolved_error is not None:
        return resolved_error
    assert resolved is not None
    if resolved != source_commit:
        return "budget summary source_commit does not resolve to the exact recorded commit"

    merge_base_raw, git_error = _git_bytes(
        repository, "merge-base", source_commit, head
    )
    if git_error is not None:
        return "budget summary source_commit ancestry could not be verified"
    assert merge_base_raw is not None
    merge_base, merge_base_error = _canonical_git_object_id(
        merge_base_raw, "budget summary source-to-release merge base"
    )
    if merge_base_error is not None:
        return merge_base_error
    if merge_base != source_commit:
        return "budget summary source_commit is not an ancestor of release HEAD"

    if source_commit != head:
        changed_raw, git_error = _git_bytes(
            repository,
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            source_commit,
            head,
        )
        if git_error is not None:
            return (
                "budget summary source-to-release changes could not be inspected: "
                f"{git_error}"
            )
        assert changed_raw is not None
        changed_paths = tuple(
            os.fsdecode(path) for path in changed_raw.split(b"\0") if path
        )
        if not changed_paths:
            return "budget summary source_commit differs from HEAD without a source diff"

        cargo_raw, git_error = _git_bytes(
            repository, "show", f"{source_commit}:Cargo.toml"
        )
        if git_error is not None:
            return f"source Cargo.toml package policy could not be inspected: {git_error}"
        assert cargo_raw is not None
        try:
            cargo_document = tomllib.loads(cargo_raw.decode("utf-8", "strict"))
        except (UnicodeError, tomllib.TOMLDecodeError) as exc:
            return f"source Cargo.toml package policy could not be parsed: {exc}"
        package_document = cargo_document.get("package", {})
        if not isinstance(package_document, dict):
            return "source Cargo.toml package table must be an object"
        package_patterns = package_document.get("include")

        allowed_prefixes = (
            "tests/perf/reports/",
            "tests/e2e_results/",
            "tests/ext_conformance/reports/",
            "tests/certification/",
            "docs/evidence/",
        )
        for path in changed_paths:
            if not path.startswith(allowed_prefixes):
                return f"non-evidence path changed after budget summary source_commit: {path}"
            try:
                packaged = path.startswith("docs/evidence/") and _package_includes(
                    path, package_patterns
                )
            except PerformanceContractError as exc:
                return str(exc)
            if packaged:
                return (
                    "packaged or product-consumed evidence changed after source_commit: "
                    f"{path}"
                )

    final_head_raw, git_error = _git_bytes(
        repository, "rev-parse", "--verify", "HEAD^{commit}"
    )
    if git_error is not None:
        return f"budget summary release HEAD could not be revalidated: {git_error}"
    assert final_head_raw is not None
    final_head, final_head_error = _canonical_git_object_id(
        final_head_raw, "budget summary final release HEAD"
    )
    if final_head_error is not None:
        return final_head_error
    if final_head != head:
        return (
            "budget summary release HEAD changed during source-binding verification "
            f"(started={head}, ended={final_head})"
        )
    final_clean_error = _repository_clean_state_error(repository)
    if final_clean_error is not None:
        return (
            "budget summary repository changed during source-binding verification: "
            f"{final_clean_error}"
        )
    try:
        final_current_metadata = full_path.lstat()
        final_current_bytes = full_path.read_bytes()
    except OSError as exc:
        return f"budget summary current bytes could not be re-read: {exc}"
    if (
        not stat.S_ISREG(final_current_metadata.st_mode)
        or _worktree_git_mode(final_current_metadata) != metadata_parts[0]
        or final_current_bytes != head_bytes
    ):
        return "performance summary mode or bytes changed during source-binding verification"
    return None


def _canonical_budget_inventory_json(budgets: list[dict[str, object]]) -> str:
    records: list[str] = []
    for budget in budgets:
        threshold = float(budget["threshold"])
        if threshold != round(threshold, 6):
            raise PerformanceContractError(
                f"budget {budget['name']} threshold exceeds canonical six-decimal precision"
            )
        records.append(
            "{" + ",".join(
                (
                    '"name":' + json.dumps(budget["name"], ensure_ascii=False),
                    '"category":' + json.dumps(budget["category"], ensure_ascii=False),
                    '"metric":' + json.dumps(budget["metric"], ensure_ascii=False),
                    '"unit":' + json.dumps(budget["unit"], ensure_ascii=False),
                    f'"threshold":{threshold:.6f}',
                    '"comparison":' + json.dumps(budget["comparison"], ensure_ascii=False),
                    '"ci_enforced":' + ("true" if budget["ci_enforced"] else "false"),
                    '"methodology":' + json.dumps(budget["methodology"], ensure_ascii=False),
                )
            ) + "}"
        )
    return "[" + ",".join(records) + "]"


def performance_budget_claim_errors(
    repo_root: Path,
    payload: dict[str, object],
    now: datetime,
    artifact_repo_path: str,
) -> tuple[str, ...]:
    """Validate exact v2 details and source binding before authorizing a claim."""
    def fail(message: str) -> None:
        raise PerformanceContractError(message)

    def exact_fields(
        value: object,
        expected: frozenset[str],
        label: str,
        optional: frozenset[str] = frozenset(),
    ) -> dict[str, object]:
        if not isinstance(value, dict):
            fail(f"{label} must be an object")
        actual = set(value)
        missing = expected - actual
        extra = actual - expected - optional
        if missing or extra:
            fail(
                f"{label} fields are not exact "
                f"(missing={sorted(missing)}, unexpected={sorted(extra)})"
            )
        return value

    def nonempty_string(value: object, label: str) -> str:
        if not isinstance(value, str) or not value.strip() or value != value.strip():
            fail(f"{label} must be a non-empty, surrounding-whitespace-free string")
        return value

    def uint(value: object, label: str) -> int:
        if (
            isinstance(value, bool)
            or not isinstance(value, int)
            or not 0 <= value <= 2**63 - 1
        ):
            fail(f"{label} must be a non-negative signed 64-bit integer")
        return value

    def finite_number(value: object, label: str, *, positive: bool = False) -> float:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            fail(f"{label} must be a finite number")
        try:
            result = float(value)
        except (OverflowError, ValueError):
            qualifier = "positive finite" if positive else "finite"
            fail(f"{label} must be a {qualifier} number")
        if not math.isfinite(result) or (positive and result <= 0.0):
            qualifier = "positive finite" if positive else "finite"
            fail(f"{label} must be a {qualifier} number")
        return result

    def nullable_lineage(value: object, label: str) -> str | None:
        if value is None:
            return None
        if not isinstance(value, str) or PERF_LINEAGE_RE.fullmatch(value) is None:
            fail(f"{label} must be null or a canonical lineage identifier")
        return value

    try:
        data = exact_fields(payload, PERF_SUMMARY_FIELDS, "performance summary")
        if data["schema"] != PERF_BUDGET_SUMMARY_SCHEMA:
            fail(f"unsupported performance summary schema: {data['schema']!r}")

        generated_at_raw = data["generated_at"]
        if (
            not isinstance(generated_at_raw, str)
            or PERF_TIMESTAMP_RE.fullmatch(generated_at_raw) is None
        ):
            fail("generated_at must use canonical millisecond-precision UTC RFC3339")
        generated_at = parse_iso_datetime(generated_at_raw)
        if generated_at is None:
            fail("generated_at must use canonical millisecond-precision UTC RFC3339")
        if generated_at > as_utc(now) + timedelta(minutes=5):
            fail("generated_at is more than five minutes in the future")

        source_commit_value = data["source_commit"]
        if source_commit_value is not None and (
            not isinstance(source_commit_value, str)
            or PERF_OBJECT_ID_RE.fullmatch(source_commit_value) is None
            or set(source_commit_value) == {"0"}
        ):
            fail("source_commit must be null or a canonical full lowercase Git object ID")
        source_commit = source_commit_value if isinstance(source_commit_value, str) else None

        run_id = nullable_lineage(data["run_id"], "run_id")
        correlation_id = nullable_lineage(data["correlation_id"], "correlation_id")
        if run_id != correlation_id:
            fail("run_id and correlation_id must both be null or match")
        strict_mode = data["strict_mode"]
        if not isinstance(strict_mode, bool):
            fail("strict_mode must be a boolean")

        count_names = (
            "total_budgets",
            "ci_enforced",
            "ci_with_data",
            "ci_fail",
            "ci_no_data",
            "pass",
            "fail",
            "no_data",
            "data_contract_failures_count",
        )
        counts = {name: uint(data[name], name) for name in count_names}

        budgets_value = data["budgets"]
        results_value = data["budget_results"]
        failures_value = data["failing_data_contracts"]
        if not isinstance(budgets_value, list) or not budgets_value:
            fail("budgets must be a non-empty array")
        if not isinstance(results_value, list) or not results_value:
            fail("budget_results must be a non-empty array")
        if not isinstance(failures_value, list):
            fail("failing_data_contracts must be an array")

        budgets: list[dict[str, object]] = []
        budgets_by_name: dict[str, dict[str, object]] = {}
        for index, raw_budget in enumerate(budgets_value):
            budget = exact_fields(raw_budget, PERF_BUDGET_FIELDS, f"budgets[{index}]")
            name = nonempty_string(budget["name"], f"budgets[{index}].name")
            if name in budgets_by_name:
                fail(f"duplicate budget name: {name}")
            for field in ("category", "metric", "unit", "methodology"):
                nonempty_string(budget[field], f"budgets[{index}].{field}")
            finite_number(budget["threshold"], f"budgets[{index}].threshold", positive=True)
            if budget["comparison"] not in ("maximum", "minimum"):
                fail(f"budgets[{index}].comparison must be 'maximum' or 'minimum'")
            if not isinstance(budget["ci_enforced"], bool):
                fail(f"budgets[{index}].ci_enforced must be a boolean")
            budgets.append(budget)
            budgets_by_name[name] = budget

        inventory_json = _canonical_budget_inventory_json(budgets)
        inventory_sha256 = hashlib.sha256(inventory_json.encode("utf-8")).hexdigest()
        if inventory_sha256 != PERF_BUDGET_INVENTORY_SHA256:
            fail(
                "budget inventory does not match the canonical producer contract "
                f"(observed_sha256={inventory_sha256}, "
                f"expected_sha256={PERF_BUDGET_INVENTORY_SHA256})"
            )

        results_by_name: dict[str, dict[str, object]] = {}
        status_counts = {"PASS": 0, "FAIL": 0, "NO_DATA": 0}
        ci_with_data = 0
        ci_fail = 0
        ci_no_data = 0
        for index, raw_result in enumerate(results_value):
            result = exact_fields(
                raw_result,
                PERF_RESULT_REQUIRED_FIELDS,
                f"budget_results[{index}]",
                PERF_RESULT_OPTIONAL_FIELDS,
            )
            name = nonempty_string(
                result["budget_name"], f"budget_results[{index}].budget_name"
            )
            if name in results_by_name:
                fail(f"duplicate budget result: {name}")
            budget = budgets_by_name.get(name)
            if budget is None:
                fail(f"budget result has no matching definition: {name}")
            for field in ("category", "unit", "comparison"):
                nonempty_string(result[field], f"budget_results[{index}].{field}")
                if result[field] != budget[field]:
                    fail(f"budget result {name} has mismatched {field}")
            threshold = finite_number(
                result["threshold"], f"budget_results[{index}].threshold", positive=True
            )
            if threshold != float(budget["threshold"]):
                fail(f"budget result {name} has mismatched threshold")
            if not isinstance(result["ci_enforced"], bool):
                fail(f"budget_results[{index}].ci_enforced must be a boolean")
            if result["ci_enforced"] is not budget["ci_enforced"]:
                fail(f"budget result {name} has mismatched ci_enforced")
            nonempty_string(result["source"], f"budget_results[{index}].source")
            status = result["status"]
            if not isinstance(status, str) or status not in status_counts:
                fail(f"budget result {name} has unsupported status: {status!r}")
            failure_reason_present = "failure_reason" in result
            failure_reason = result.get("failure_reason")
            if failure_reason_present:
                nonempty_string(
                    failure_reason, f"budget_results[{index}].failure_reason"
                )
            actual = result["actual"]
            if actual is None:
                if strict_mode and budget["ci_enforced"]:
                    if status != "FAIL" or failure_reason != "missing_measurement_data":
                        fail(
                            f"strict CI budget {name} without data must be FAIL with "
                            "failure_reason=missing_measurement_data"
                        )
                elif status != "NO_DATA" or failure_reason_present:
                    fail(
                        f"budget {name} without data must be NO_DATA without a failure reason"
                    )
            else:
                actual_value = finite_number(
                    actual, f"budget_results[{index}].actual"
                )
                if actual_value < 0.0:
                    fail(f"budget_results[{index}].actual must be non-negative")
                passes = (
                    actual_value >= threshold
                    if budget["comparison"] == "minimum"
                    else actual_value <= threshold
                )
                expected_status = "PASS" if passes else "FAIL"
                if status != expected_status:
                    fail(
                        f"budget result {name} status={status} is inconsistent with "
                        f"actual={actual_value} and threshold={threshold}"
                    )
                if failure_reason_present:
                    fail(f"budget result {name} with data must not contain failure_reason")
            status_counts[status] += 1
            if budget["ci_enforced"]:
                ci_with_data += int(actual is not None)
                ci_fail += int(status == "FAIL")
                ci_no_data += int(status == "NO_DATA")
            results_by_name[name] = result

        if list(results_by_name) != list(budgets_by_name):
            missing = sorted(set(budgets_by_name) - set(results_by_name))
            unexpected = sorted(set(results_by_name) - set(budgets_by_name))
            fail(
                "budget_results must match canonical budget declaration order and membership "
                f"(missing={missing}, unexpected={unexpected})"
            )

        failure_fingerprints: set[tuple[str, str, str, str | None]] = set()
        for index, raw_failure in enumerate(failures_value):
            failure = exact_fields(
                raw_failure,
                PERF_FAILURE_REQUIRED_FIELDS,
                f"failing_data_contracts[{index}]",
                PERF_FAILURE_OPTIONAL_FIELDS,
            )
            contract_id = nonempty_string(
                failure["contract_id"], f"failing_data_contracts[{index}].contract_id"
            )
            detail = nonempty_string(
                failure["detail"], f"failing_data_contracts[{index}].detail"
            )
            remediation = nonempty_string(
                failure["remediation"], f"failing_data_contracts[{index}].remediation"
            )
            raw_budget_name = failure.get("budget_name")
            budget_name = None
            if raw_budget_name is not None:
                budget_name = nonempty_string(
                    raw_budget_name,
                    f"failing_data_contracts[{index}].budget_name",
                )
                if budget_name not in budgets_by_name:
                    fail(f"data-contract failure references unknown budget: {budget_name}")
            fingerprint = (contract_id, detail, remediation, budget_name)
            if fingerprint in failure_fingerprints:
                fail(f"duplicate data-contract failure at index {index}")
            failure_fingerprints.add(fingerprint)

        derived_counts = {
            "total_budgets": len(budgets),
            "ci_enforced": sum(int(budget["ci_enforced"]) for budget in budgets),
            "ci_with_data": ci_with_data,
            "ci_fail": ci_fail,
            "ci_no_data": ci_no_data,
            "pass": status_counts["PASS"],
            "fail": status_counts["FAIL"],
            "no_data": status_counts["NO_DATA"],
            "data_contract_failures_count": len(failures_value),
        }
        for name, expected in derived_counts.items():
            if counts[name] != expected:
                fail(f"{name}={counts[name]} is inconsistent with derived value {expected}")
        if counts["pass"] + counts["fail"] + counts["no_data"] != counts["total_budgets"]:
            fail("pass + fail + no_data must equal total_budgets")

        claim = exact_fields(
            data["claim_readiness"],
            PERF_CLAIM_READINESS_FIELDS,
            "claim_readiness",
        )
        reasons = claim["blocking_reason_codes"]
        if not isinstance(reasons, list) or any(
            not isinstance(reason, str) or not reason for reason in reasons
        ):
            fail(
                "claim_readiness.blocking_reason_codes must be an array of non-empty strings"
            )
        if reasons != sorted(set(reasons)):
            fail("claim_readiness.blocking_reason_codes must be sorted and duplicate-free")

        expected_reasons: list[str] = []
        if counts["no_data"] != 0:
            expected_reasons.append("budget_data_missing")
        if counts["fail"] != 0:
            expected_reasons.append("budget_failed")
        if counts["ci_with_data"] != counts["ci_enforced"] or counts["ci_no_data"] != 0:
            expected_reasons.append("ci_budget_data_missing")
        if counts["ci_fail"] != 0:
            expected_reasons.append("ci_budget_failed")
        if correlation_id is None:
            expected_reasons.append("correlation_id_missing")
        if counts["data_contract_failures_count"] != 0:
            expected_reasons.append("data_contract_failure")
        if run_id is None:
            expected_reasons.append("run_id_missing")
        if source_commit is None:
            expected_reasons.append("source_commit_unbound")
        if not strict_mode:
            expected_reasons.append("strict_mode_disabled")
        expected_reasons.sort()
        if reasons != expected_reasons:
            fail(
                "claim_readiness.blocking_reason_codes do not match derived blockers "
                f"(reported={reasons}, expected={expected_reasons})"
            )

        claim_ready = not expected_reasons
        expected_status = "claim_ready" if claim_ready else "blocked"
        if claim["status"] != expected_status:
            fail(f"claim_readiness.status must be {expected_status!r}")
        if claim["performance_claims_authorized"] is not claim_ready:
            fail(f"claim_readiness.performance_claims_authorized must be {claim_ready}")
        if not claim_ready:
            fail(
                "budget summary does not authorize release-facing performance claims: "
                f"blocking_reason_codes={expected_reasons!r}"
            )
        assert source_commit is not None
        binding_error = performance_source_binding_error(
            repo_root, source_commit, artifact_repo_path
        )
        if binding_error is not None:
            fail(binding_error)
    except PerformanceContractError as exc:
        return (str(exc),)
    return ()


def check_artifact_content(
    repo_root: Path,
    artifact_path: str,
    cited_artifact_path: str,
    citation_kind: str,
    citation_value: str,
    artifact_bytes: bytes,
    now: datetime,
    staleness_threshold: timedelta,
    claim_surface: str,
) -> tuple[str, ...]:
    """Validate that a cited artifact actually supports the README claim."""
    errors: list[str] = []
    text, read_error = decode_artifact_text(artifact_bytes)
    if read_error is not None:
        return (read_error,)
    assert text is not None

    payload, json_error = load_json_object(artifact_path, text)
    if json_error is not None:
        errors.append(json_error)
        return tuple(errors)
    if payload is None:
        if citation_value not in text:
            errors.append(
                f"cited {citation_kind} {citation_value!r} not found in artifact content"
            )
        return tuple(errors)

    if citation_kind == "run":
        structured_ids = {
            field: payload[field]
            for field in ("run_id", "correlation_id")
            if field in payload
        }
        if not structured_ids:
            errors.append(
                "JSON artifact must expose structured run_id or correlation_id provenance"
            )
        else:
            for field, structured_value in structured_ids.items():
                if structured_value != citation_value:
                    errors.append(
                        f"cited run {citation_value!r} does not exactly match "
                        f"artifact {field}={structured_value!r}"
                    )
    elif citation_kind == "generated":
        cited_generated_at = canonicalize_iso_datetime(citation_value)
        structured_generated_at = canonicalize_iso_datetime(payload.get("generated_at"))
        if cited_generated_at is None:
            errors.append(f"cited generated timestamp is not parseable: {citation_value!r}")
        if structured_generated_at is None:
            errors.append("JSON artifact missing parseable generated_at timestamp")
        elif (
            cited_generated_at is not None
            and cited_generated_at != structured_generated_at
        ):
            errors.append(
                f"cited generated timestamp {citation_value!r} does not exactly match "
                f"artifact generated_at={payload.get('generated_at')!r} after normalization"
            )
    else:
        errors.append(f"unsupported citation kind: {citation_kind!r}")

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
    elif claim_surface != "historical_snapshot":
        if generated_at > now + timedelta(minutes=5):
            errors.append("artifact generated_at is more than five minutes in the future")
        elif now - generated_at > staleness_threshold:
            days_old = (now - generated_at).total_seconds() / 86400
            errors.append(f"artifact generated_at is stale: {days_old:.1f} days old")

    if (
        artifact_path == "tests/perf/reports/budget_summary.json"
        and claim_surface == "release_facing"
    ):
        errors.extend(
            performance_budget_claim_errors(
                repo_root, payload, now, cited_artifact_path
            )
        )

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
    quantitative_performance_claims = parse_quantitative_performance_claims(readme_text)
    uncited_quantitative_claims = [
        claim
        for claim in quantitative_performance_claims
        if not claim.has_canonical_inline_citation
    ]

    for claim in uncited_quantitative_claims:
        print(
            f"INVALID: line {claim.line_number}: quantitative performance claim lacks "
            f"an inline citation to {CANONICAL_PERFORMANCE_SUMMARY_PATH}: {claim.text}"
        )
        print(
            "  Remediation: cite the canonical claim-ready performance summary on the "
            "same line, or rewrite the statement as an explicitly unmeasured target."
        )

    if not obligations:
        print("INFO: No artifact citations found in README.md")
        if claim_gated_phrases:
            print(f"INFO: Found {len(claim_gated_phrases)} claim-gated phrase(s) without hard citations")
        return 1 if uncited_quantitative_claims else 0

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
    content_error_count = len(uncited_quantitative_claims)

    for obligation in obligations:
        cited_artifact_path = obligation.artifact_path
        correlation_id = obligation.citation_value
        artifact_path, full_path, path_error = resolve_repo_artifact_path(
            repo_root, cited_artifact_path
        )
        if path_error is not None:
            content_error_count += 1
            print(
                f"INVALID: line {obligation.line_number}: "
                f"{cited_artifact_path}: {path_error}"
            )
            print(
                "  Remediation: cite a contained repository-relative artifact path "
                f"on line {obligation.line_number}."
            )
            results.append(CitationCheck(
                artifact_path=cited_artifact_path,
                correlation_id=correlation_id,
                line_number=obligation.line_number,
                claim_surface=obligation.claim_surface,
                file_exists=False,
                file_mtime=None,
                days_old=None,
                is_stale=False,
                content_errors=(path_error,),
            ))
            continue
        assert artifact_path is not None and full_path is not None
        claim_surface = classify_claim_surface(obligation.claim_text, artifact_path)

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
                claim_surface=claim_surface,
                file_exists=False,
                file_mtime=None,
                days_old=None,
                is_stale=False,
                content_errors=(),
            ))
            continue

        release_snapshot: ReleaseArtifactSnapshot | None = None
        release_binding_errors: list[str] = []
        if claim_surface == "release_facing":
            release_snapshot, release_binding_error = capture_release_artifact_snapshot(
                repo_root, artifact_path
            )
            if release_binding_error is not None:
                release_binding_errors.append(release_binding_error)

        try:
            if release_snapshot is not None:
                artifact_bytes = release_snapshot.contents
                mtime = release_snapshot.file_mtime
            else:
                metadata = full_path.lstat()
                if not stat.S_ISREG(metadata.st_mode):
                    raise OSError("artifact is not a regular file")
                artifact_bytes = full_path.read_bytes()
                mtime = datetime.fromtimestamp(metadata.st_mtime, timezone.utc)
            freshness_time = mtime
            freshness_source = "filesystem_mtime"
            head_commit_time: datetime | None = None
            if (
                release_snapshot is not None
                and not artifact_path.endswith(".json")
            ):
                head_commit_time, commit_time_error = (
                    release_artifact_head_commit_time(release_snapshot)
                )
                if commit_time_error is not None:
                    release_binding_errors.append(commit_time_error)
                    freshness_source = "git_head_path_commit_unresolved"
                else:
                    assert head_commit_time is not None
                    freshness_time = head_commit_time
                    freshness_source = "git_head_path_commit"

            age = now - freshness_time
            days_old = age.total_seconds() / 86400  # Convert to days
            is_stale = claim_surface != "historical_snapshot" and age > staleness_threshold
            if (
                claim_surface == "release_facing"
                and mtime > now + MAX_FUTURE_CLOCK_SKEW
            ):
                release_binding_errors.append(
                    "artifact modification time is more than five minutes in the future"
                )
            if (
                head_commit_time is not None
                and head_commit_time > now + MAX_FUTURE_CLOCK_SKEW
            ):
                release_binding_errors.append(
                    "artifact HEAD-bound commit time is more than five minutes in the future"
                )

            if is_stale:
                print(
                    f"STALE: line {obligation.line_number}: {artifact_path} "
                    f"(age: {days_old:.1f} days, limit: 14 days, "
                    f"freshness={freshness_source})"
                )
                print(
                    "  Remediation: regenerate fresh evidence and update the README citation "
                    f"run/provenance on line {obligation.line_number}."
                )
                stale_count += 1
            else:
                freshness_label = "HISTORICAL" if claim_surface == "historical_snapshot" else "FRESH"
                print(
                    f"{freshness_label}: line {obligation.line_number}: {artifact_path} "
                    f"(age: {days_old:.1f} days, surface={claim_surface}, "
                    f"proof={proof_artifact_family(artifact_path)}, "
                    f"freshness={freshness_source})"
                )

            artifact_content_errors = check_artifact_content(
                repo_root,
                artifact_path,
                cited_artifact_path,
                obligation.citation_kind,
                correlation_id,
                artifact_bytes,
                now,
                staleness_threshold,
                claim_surface,
            )
            if release_snapshot is not None:
                final_binding_error = revalidate_release_artifact_snapshot(
                    release_snapshot
                )
                if final_binding_error is not None:
                    release_binding_errors.append(final_binding_error)
            content_errors = tuple(release_binding_errors) + artifact_content_errors
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
                claim_surface=claim_surface,
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
    print(f"  Quantitative performance claims extracted: {len(quantitative_performance_claims)}")
    print(f"  Quantitative performance claims missing canonical citations: {len(uncited_quantitative_claims)}")
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
    """Exercise citation, path, detailed budget, and Git-binding contracts."""
    now = datetime(2026, 5, 1, 12, 0, 0, tzinfo=timezone.utc)
    canonical_generated_at = now.isoformat(timespec="milliseconds").replace("+00:00", "Z")
    fresh_ts = now.timestamp()

    project_root = Path(__file__).resolve().parent.parent
    canonical_summary_path = project_root / "tests/perf/reports/budget_summary.json"
    try:
        canonical_summary = json.loads(canonical_summary_path.read_text(encoding="utf-8"))
        canonical_budgets = canonical_summary["budgets"]
    except (OSError, KeyError, json.JSONDecodeError) as exc:
        print(f"SELF-TEST FAIL: cannot load canonical budget fixture: {exc}")
        return 2
    if not isinstance(canonical_budgets, list):
        print("SELF-TEST FAIL: canonical budget fixture must contain a budgets array")
        return 2

    def cloned(value: object) -> object:
        return json.loads(json.dumps(value))

    def run_check(repo_root: Path, readme_text: str) -> tuple[int, str]:
        (repo_root / "README.md").write_text(readme_text, encoding="utf-8")
        if (repo_root / ".git").is_dir():
            git(repo_root, "add", "--all")
            if git(repo_root, "status", "--porcelain"):
                git(repo_root, "commit", "-q", "-m", "generic evidence fixture")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            result = check_readme(repo_root, now=now)
        return result, output.getvalue()

    def git(repo_root: Path, *args: str, env: dict[str, str] | None = None) -> bytes:
        result = subprocess.run(
            ["git", "-C", str(repo_root), *args],
            check=False,
            capture_output=True,
            env=env,
        )
        if result.returncode != 0:
            raise RuntimeError(
                f"git {' '.join(args)} failed: "
                f"{result.stderr.decode('utf-8', 'replace').strip()}"
            )
        return result.stdout

    def commit_fixture_paths_at(
        repo_root: Path,
        timestamp: datetime,
        message: str,
        *paths: str,
    ) -> None:
        git(repo_root, "add", "--", *paths)
        commit_environment = os.environ.copy()
        commit_timestamp = as_utc(timestamp).isoformat()
        commit_environment["GIT_AUTHOR_DATE"] = commit_timestamp
        commit_environment["GIT_COMMITTER_DATE"] = commit_timestamp
        git(
            repo_root,
            "-c",
            "commit.gpgSign=false",
            "commit",
            "-q",
            "-m",
            message,
            env=commit_environment,
        )

    def claim_ready_payload(source_commit: str) -> dict[str, object]:
        budgets = cloned(canonical_budgets)
        assert isinstance(budgets, list)
        results: list[dict[str, object]] = []
        for raw_budget in budgets:
            assert isinstance(raw_budget, dict)
            results.append(
                {
                    "budget_name": raw_budget["name"],
                    "category": raw_budget["category"],
                    "threshold": raw_budget["threshold"],
                    "comparison": raw_budget["comparison"],
                    "unit": raw_budget["unit"],
                    "actual": raw_budget["threshold"],
                    "status": "PASS",
                    "source": "fixture://canonical-measurement",
                    "ci_enforced": raw_budget["ci_enforced"],
                }
            )
        ci_enforced = sum(
            int(bool(budget["ci_enforced"]))
            for budget in budgets
            if isinstance(budget, dict)
        )
        return {
            "schema": PERF_BUDGET_SUMMARY_SCHEMA,
            "generated_at": canonical_generated_at,
            "source_commit": source_commit,
            "run_id": "budget-run",
            "correlation_id": "budget-run",
            "strict_mode": True,
            "total_budgets": len(budgets),
            "ci_enforced": ci_enforced,
            "ci_with_data": ci_enforced,
            "ci_fail": 0,
            "ci_no_data": 0,
            "pass": len(budgets),
            "fail": 0,
            "no_data": 0,
            "data_contract_failures_count": 0,
            "failing_data_contracts": [],
            "budgets": budgets,
            "budget_results": results,
            "claim_readiness": {
                "status": "claim_ready",
                "performance_claims_authorized": True,
                "blocking_reason_codes": [],
            },
        }

    def create_binding_repo(
        base: Path,
        name: str,
        package_include: str | None = 'include = ["Cargo.toml", "README.md", "src/**"]',
    ) -> tuple[Path, dict[str, object], str]:
        repo_root = base / name
        repo_root.mkdir()
        git(repo_root, "init", "-q", "-b", "main")
        git(repo_root, "config", "user.name", "README Evidence Self-Test")
        git(repo_root, "config", "user.email", "readme-evidence@example.invalid")
        manifest_lines = [
            "[package]",
            'name = "readme-evidence-fixture"',
            'version = "0.1.0"',
            'edition = "2024"',
        ]
        if package_include is not None:
            manifest_lines.append(package_include)
        manifest_lines.append("")
        (repo_root / "Cargo.toml").write_text(
            "\n".join(manifest_lines),
            encoding="utf-8",
        )
        (repo_root / "README.md").write_text(
            "Claim: *(from tests/perf/reports/budget_summary.json, run budget-run)*\n",
            encoding="utf-8",
        )
        source_file = repo_root / "src/lib.rs"
        source_file.parent.mkdir()
        source_file.write_text("pub fn fixture() {}\n", encoding="utf-8")
        git(repo_root, "add", "Cargo.toml", "README.md", "src/lib.rs")
        git(repo_root, "commit", "-q", "-m", "source")
        source_commit = git(repo_root, "rev-parse", "HEAD").decode("ascii").strip()

        payload = claim_ready_payload(source_commit)
        artifact = repo_root / "tests/perf/reports/budget_summary.json"
        artifact.parent.mkdir(parents=True)
        artifact.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        git(repo_root, "add", "tests/perf/reports/budget_summary.json")
        git(repo_root, "commit", "-q", "-m", "evidence")
        os.utime(artifact, (fresh_ts, fresh_ts))
        return repo_root, payload, source_commit

    with TemporaryDirectory() as temp_dir:
        base = Path(temp_dir)
        generic_root = base / "generic"
        generic_root.mkdir()
        git(generic_root, "init", "-q", "-b", "main")
        git(generic_root, "config", "user.name", "README Evidence Self-Test")
        git(
            generic_root,
            "config",
            "user.email",
            "readme-evidence@example.invalid",
        )
        reports = generic_root / "tests/perf/reports"
        reports.mkdir(parents=True)

        fresh_artifact = reports / "fresh.json"
        fresh_artifact.write_text(
            json.dumps(
                {
                    "generated_at": canonical_generated_at,
                    "correlation_id": "fixture-run",
                    "ok": True,
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(fresh_artifact, (fresh_ts, fresh_ts))
        result, output = run_check(
            generic_root,
            "\n".join(
                (
                    "Example: `*(from [artifact-path], run [correlation-id])*`",
                    "```",
                    "*(from missing-in-code-block.json, run example)*",
                    "```",
                    "Claim: *(from tests/perf/reports/fresh.json, run fixture-run)*",
                    "",
                )
            ),
        )
        if result != 0:
            print(output)
            print("SELF-TEST FAIL: exact structured run citation should pass")
            return 2
        if "[artifact-path]" in output or "missing-in-code-block" in output:
            print(output)
            print("SELF-TEST FAIL: examples/placeholders must not be parsed as claims")
            return 2
        obligations = parse_citation_obligations(
            (generic_root / "README.md").read_text(encoding="utf-8")
        )
        if len(obligations) != 1 or obligations[0].line_number != 5:
            print(obligations)
            print("SELF-TEST FAIL: citation obligations must retain README line numbers")
            return 2

        generated_artifact = reports / "generated.json"
        generated_artifact.write_text(
            json.dumps({"generated_at": canonical_generated_at, "ok": True}) + "\n",
            encoding="utf-8",
        )
        os.utime(generated_artifact, (fresh_ts, fresh_ts))
        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/generated.json, generated "
            "`2026-05-01T08:00:00.000-04:00`)*\n",
        )
        if result != 0:
            print(output)
            print("SELF-TEST FAIL: canonically equivalent generated citation should pass")
            return 2

        run_collision = reports / "run_collision.json"
        run_collision.write_text(
            json.dumps(
                {
                    "generated_at": canonical_generated_at,
                    "run_id": "prefix-cited-run-suffix",
                    "correlation_id": "prefix-cited-run-suffix",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(run_collision, (fresh_ts, fresh_ts))
        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/run_collision.json, run cited-run)*\n",
        )
        if result != 1 or "does not exactly match artifact run_id" not in output:
            print(output)
            print("SELF-TEST FAIL: run substring collision must not prove provenance")
            return 2

        generated_collision = reports / "generated_collision.json"
        generated_collision.write_text(
            json.dumps({"generated_at": "2026-05-01T12:00:00.123Z"}) + "\n",
            encoding="utf-8",
        )
        os.utime(generated_collision, (fresh_ts, fresh_ts))
        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/generated_collision.json, generated "
            "2026-05-01T12:00:00)*\n",
        )
        if result != 1 or "does not exactly match artifact generated_at" not in output:
            print(output)
            print("SELF-TEST FAIL: generated substring collision must not prove provenance")
            return 2

        stale_artifact = reports / "stale.json"
        stale_artifact.write_text(
            json.dumps(
                {
                    "generated_at": (now - timedelta(days=30)).isoformat(),
                    "correlation_id": "stale-run",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        os.utime(stale_artifact, (fresh_ts, fresh_ts))
        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/stale.json, run stale-run)*\n",
        )
        if result != 1 or "artifact generated_at is stale" not in output:
            print(output)
            print("SELF-TEST FAIL: stale generated_at must fail despite fresh mtime")
            return 2

        historical = generic_root / "docs/planning/historical_snapshot.json"
        historical.parent.mkdir(parents=True)
        historical.write_text(
            json.dumps(
                {
                    "generated_at": (now - timedelta(days=45)).isoformat(),
                    "correlation_id": "historical-run",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        old_ts = (now - timedelta(days=45)).timestamp()
        os.utime(historical, (old_ts, old_ts))
        result, output = run_check(
            generic_root,
            "Historical-only; not a current release claim: benchmark snapshot "
            "*(from docs/planning/historical_snapshot.json, run historical-run)*\n",
        )
        if result != 0 or "surface=historical_snapshot" not in output:
            print(output)
            print("SELF-TEST FAIL: historical freshness behavior must remain intentional")
            return 2

        historical_keyword_bypasses = (
            "Startup is 5 ms versus the baseline",
            "The retained result shows startup is 5 ms",
            "Current release snapshot: startup is 5 ms",
            "Historical benchmark shows the current release starts in 5 ms",
            "Not treated as current by the baseline; startup is 5 ms",
            "This says historical-only and not a current release claim: startup is 5 ms",
            "Not historical-only; not a current release claim: startup is 5 ms",
        )
        for claim in historical_keyword_bypasses:
            if (
                classify_claim_surface(
                    claim,
                    "tests/perf/reports/budget_summary.json",
                )
                != "release_facing"
            ):
                print(
                    "SELF-TEST FAIL: generic historical vocabulary must not "
                    f"downgrade a current quantitative claim: {claim!r}"
                )
                return 2
        malformed_historical_disclaimers = (
            "This says historical-only and not a current release claim: startup is 5 ms",
            "Not historical-only; not a current release claim: startup is 5 ms",
            "Historical-only — not a current release claim: startup is 5 ms",
            "Historical-only; not a current release claim: benchmark snapshot. "
            "Current release starts in 5 ms",
        )
        for claim in malformed_historical_disclaimers:
            if (
                classify_claim_surface(
                    claim,
                    "docs/planning/historical_snapshot.json",
                )
                != "release_facing"
            ):
                print(
                    "SELF-TEST FAIL: only the exact leading historical disclaimer "
                    f"may downgrade a planning citation: {claim!r}"
                )
                return 2
        if (
            classify_claim_surface(
                "Historical-only; not a current release claim: old result",
                "tests/perf/reports/budget_summary.json",
            )
            != "release_facing"
        ):
            print(
                "SELF-TEST FAIL: the historical-only exemption must be limited "
                "to planning artifacts"
            )
            return 2

        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/missing.json, run fixture-run)*\n",
        )
        if result != 1 or "cited artifact does not exist" not in output:
            print(output)
            print("SELF-TEST FAIL: missing citation should fail")
            return 2

        outside = base / "outside.json"
        outside.write_text(
            json.dumps(
                {
                    "generated_at": canonical_generated_at,
                    "correlation_id": "outside-run",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        result, output = run_check(
            generic_root,
            "Claim: *(from ../outside.json, run outside-run)*\n",
        )
        if result != 1 or "artifact path must be canonical" not in output:
            print(output)
            print("SELF-TEST FAIL: parent traversal must be rejected before reading")
            return 2

        escape_link = reports / "escape.json"
        escape_link.symlink_to(outside)
        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/escape.json, run outside-run)*\n",
        )
        if result != 1 or "symlink components" not in output:
            print(output)
            print("SELF-TEST FAIL: symlink escape must be rejected before reading")
            return 2

        internal_target = reports / "internal-target.json"
        internal_target.write_text(
            json.dumps(
                {
                    "generated_at": canonical_generated_at,
                    "correlation_id": "internal-run",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        internal_link = reports / "internal-link.json"
        internal_link.symlink_to("internal-target.json")
        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/internal-link.json, run internal-run)*\n",
        )
        if result != 1 or "symlink components" not in output:
            print(output)
            print("SELF-TEST FAIL: contained artifact symlinks must fail closed")
            return 2

        future_artifact = reports / "future.json"
        future_artifact.write_text(
            json.dumps(
                {
                    "generated_at": (now + timedelta(minutes=6)).isoformat(),
                    "correlation_id": "future-run",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/future.json, run future-run)*\n",
        )
        if result != 1 or "more than five minutes in the future" not in output:
            print(output)
            print("SELF-TEST FAIL: future-dated release evidence must fail closed")
            return 2

        ignored_artifact = reports / "ignored.json"
        (generic_root / ".git/info/exclude").write_text(
            "tests/perf/reports/ignored.json\n",
            encoding="utf-8",
        )
        ignored_artifact.write_text(
            json.dumps(
                {
                    "generated_at": canonical_generated_at,
                    "correlation_id": "ignored-run",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        ignored_binding_error = release_artifact_head_binding_error(
            generic_root, "tests/perf/reports/ignored.json"
        )
        if ignored_binding_error is None or "tracked at HEAD" not in ignored_binding_error:
            print(ignored_binding_error)
            print("SELF-TEST FAIL: ignored release evidence must not authorize claims")
            return 2

        fresh_text_artifact = generic_root / "docs/evidence/fresh.txt"
        fresh_text_artifact.parent.mkdir(parents=True, exist_ok=True)
        fresh_text_artifact.write_text("fresh-text-run\n", encoding="utf-8")
        commit_fixture_paths_at(
            generic_root,
            now,
            "fresh non-JSON evidence",
            "docs/evidence/fresh.txt",
        )
        stale_mtime = (now - timedelta(days=30)).timestamp()
        os.utime(fresh_text_artifact, (stale_mtime, stale_mtime))
        result, output = run_check(
            generic_root,
            "Claim: *(from docs/evidence/fresh.txt, run fresh-text-run)*\n",
        )
        if result != 0 or "freshness=git_head_path_commit" not in output:
            print(output)
            print(
                "SELF-TEST FAIL: fresh committed non-JSON evidence must use its "
                "HEAD-bound commit time, not stale filesystem metadata"
            )
            return 2

        touched_stale_artifact = generic_root / "docs/evidence/touched-stale.txt"
        touched_stale_artifact.write_text("touched-stale-run\n", encoding="utf-8")
        commit_fixture_paths_at(
            generic_root,
            now - timedelta(days=30),
            "stale non-JSON evidence",
            "docs/evidence/touched-stale.txt",
        )
        os.utime(touched_stale_artifact, (fresh_ts, fresh_ts))
        result, output = run_check(
            generic_root,
            "Claim: *(from docs/evidence/touched-stale.txt, run touched-stale-run)*\n",
        )
        if (
            result != 1
            or "STALE:" not in output
            or "freshness=git_head_path_commit" not in output
        ):
            print(output)
            print(
                "SELF-TEST FAIL: touching stale committed non-JSON evidence must not "
                "refresh it without new byte/commit evidence"
            )
            return 2

        future_text_artifact = generic_root / "docs/evidence/future.txt"
        future_text_artifact.parent.mkdir(parents=True, exist_ok=True)
        future_text_artifact.write_text("future-text-run\n", encoding="utf-8")
        future_mtime = (now + timedelta(minutes=6)).timestamp()
        os.utime(future_text_artifact, (future_mtime, future_mtime))
        result, output = run_check(
            generic_root,
            "Claim: *(from docs/evidence/future.txt, run future-text-run)*\n",
        )
        if result != 1 or "modification time is more than five minutes" not in output:
            print(output)
            print("SELF-TEST FAIL: future-mtime non-JSON evidence must fail closed")
            return 2

        result, output = run_check(
            generic_root,
            "Claim: *(from tests/perf/reports/fresh.json, run fixture-run)*\n",
        )
        if result != 0:
            print(output)
            print("SELF-TEST FAIL: TOCTOU fixture baseline must pass")
            return 2
        original_fresh_bytes = fresh_artifact.read_bytes()
        original_capture = globals()["capture_release_artifact_snapshot"]

        def capture_then_substitute(
            fixture_root: Path,
            fixture_path: str,
        ) -> tuple[ReleaseArtifactSnapshot | None, str | None]:
            snapshot, error = original_capture(fixture_root, fixture_path)
            if snapshot is not None and fixture_path == "tests/perf/reports/fresh.json":
                snapshot.full_path.write_bytes(snapshot.contents + b" ")
            return snapshot, error

        globals()["capture_release_artifact_snapshot"] = capture_then_substitute
        try:
            output_buffer = io.StringIO()
            with contextlib.redirect_stdout(output_buffer):
                result = check_readme(generic_root, now=now)
            output = output_buffer.getvalue()
        finally:
            globals()["capture_release_artifact_snapshot"] = original_capture
            fresh_artifact.write_bytes(original_fresh_bytes)
        if result != 1 or "raw bytes changed during validation" not in output:
            print(output)
            print("SELF-TEST FAIL: validate-then-substitute TOCTOU must fail closed")
            return 2

        if os.name != "nt":
            original_mode = fresh_artifact.stat().st_mode
            os.chmod(fresh_artifact, original_mode | stat.S_IXUSR)
            output_buffer = io.StringIO()
            with contextlib.redirect_stdout(output_buffer):
                result = check_readme(generic_root, now=now)
            output = output_buffer.getvalue()
            os.chmod(fresh_artifact, original_mode)
            if result != 1 or "executable mode does not exactly match HEAD" not in output:
                print(output)
                print("SELF-TEST FAIL: live executable-mode substitution must fail closed")
                return 2

        result, output = run_check(
            generic_root,
            f"Claim: *(from {outside}, run outside-run)*\n",
        )
        if result != 1 or "must be repository-relative" not in output:
            print(output)
            print("SELF-TEST FAIL: absolute artifact path must be rejected")
            return 2

        phrases = parse_claim_gated_phrases(
            "p99 latency `example code` should stay visible when it is claim language\n"
        )
        if len(phrases) != 1 or phrases[0].phrase != "p99 latency":
            print(phrases)
            print("SELF-TEST FAIL: claim-gated performance phrases should be extracted")
            return 2

        result, output = run_check(
            generic_root,
            "Observed p99 latency is 12 ms under the canonical workload.\n",
        )
        if result != 1 or "quantitative performance claim lacks an inline citation" not in output:
            print(output)
            print("SELF-TEST FAIL: uncited quantitative performance claims must fail closed")
            return 2

        result, output = run_check(
            generic_root,
            "Target p99 latency is 12 ms; this has not been measured.\n",
        )
        if result != 0:
            print(output)
            print("SELF-TEST FAIL: explicit unmeasured performance targets are policy, not claims")
            return 2

        valid_root, valid_payload, source_commit = create_binding_repo(base, "binding-valid")
        output_buffer = io.StringIO()
        with contextlib.redirect_stdout(output_buffer):
            valid_result = check_readme(valid_root, now=now)
        if valid_result != 0:
            print(output_buffer.getvalue())
            print("SELF-TEST FAIL: exact detailed claim-ready evidence should pass")
            return 2

        incomplete = cloned(valid_payload)
        assert isinstance(incomplete, dict)
        incomplete_results = incomplete["budget_results"]
        assert isinstance(incomplete_results, list)
        incomplete_results.pop()
        errors = performance_budget_claim_errors(
            valid_root,
            incomplete,
            now,
            "tests/perf/reports/budget_summary.json",
        )
        if not any("order and membership" in error for error in errors):
            print(errors)
            print("SELF-TEST FAIL: incomplete budget_results must fail closed")
            return 2

        forged_status = cloned(valid_payload)
        assert isinstance(forged_status, dict)
        forged_results = forged_status["budget_results"]
        assert isinstance(forged_results, list) and isinstance(forged_results[0], dict)
        forged_results[0]["status"] = "FAIL"
        errors = performance_budget_claim_errors(
            valid_root,
            forged_status,
            now,
            "tests/perf/reports/budget_summary.json",
        )
        if not any("status=FAIL is inconsistent" in error for error in errors):
            print(errors)
            print("SELF-TEST FAIL: forged per-budget status must be recomputed")
            return 2

        forged_counts = cloned(valid_payload)
        assert isinstance(forged_counts, dict)
        forged_counts["pass"] = 0
        errors = performance_budget_claim_errors(
            valid_root,
            forged_counts,
            now,
            "tests/perf/reports/budget_summary.json",
        )
        if not any("inconsistent with derived value" in error for error in errors):
            print(errors)
            print("SELF-TEST FAIL: aggregate counts must be derived from detailed results")
            return 2

        forged_inventory = cloned(valid_payload)
        assert isinstance(forged_inventory, dict)
        forged_budgets = forged_inventory["budgets"]
        assert isinstance(forged_budgets, list)
        forged_budgets.reverse()
        errors = performance_budget_claim_errors(
            valid_root,
            forged_inventory,
            now,
            "tests/perf/reports/budget_summary.json",
        )
        if not any(PERF_BUDGET_INVENTORY_SHA256 in error for error in errors):
            print(errors)
            print("SELF-TEST FAIL: canonical budget inventory order/hash must be exact")
            return 2

        huge_threshold = cloned(valid_payload)
        assert isinstance(huge_threshold, dict)
        huge_budgets = huge_threshold["budgets"]
        assert isinstance(huge_budgets, list) and isinstance(huge_budgets[0], dict)
        huge_budgets[0]["threshold"] = 10**400
        errors = performance_budget_claim_errors(
            valid_root,
            huge_threshold,
            now,
            "tests/perf/reports/budget_summary.json",
        )
        if not any("threshold must be a positive finite number" in error for error in errors):
            print(errors)
            print("SELF-TEST FAIL: huge JSON numerics must fail closed without crashing")
            return 2

        forged_failures = cloned(valid_payload)
        assert isinstance(forged_failures, dict)
        forged_failures["data_contract_failures_count"] = 1
        forged_failures["failing_data_contracts"] = [
            {"contract_id": "fixture", "detail": "", "remediation": "rerun"}
        ]
        errors = performance_budget_claim_errors(
            valid_root,
            forged_failures,
            now,
            "tests/perf/reports/budget_summary.json",
        )
        if not any("detail must be a non-empty" in error for error in errors):
            print(errors)
            print("SELF-TEST FAIL: malformed failing_data_contracts must fail closed")
            return 2

        legacy = cloned(valid_payload)
        assert isinstance(legacy, dict)
        legacy["schema"] = "pi.perf.budget_summary.v1"
        errors = performance_budget_claim_errors(
            valid_root,
            legacy,
            now,
            "tests/perf/reports/budget_summary.json",
        )
        if not any("unsupported performance summary schema" in error for error in errors):
            print(errors)
            print("SELF-TEST FAIL: legacy performance schema must fail closed")
            return 2

        future = cloned(valid_payload)
        assert isinstance(future, dict)
        future["generated_at"] = (
            now + timedelta(minutes=6)
        ).isoformat(timespec="milliseconds").replace("+00:00", "Z")
        errors = performance_budget_claim_errors(
            valid_root,
            future,
            now,
            "tests/perf/reports/budget_summary.json",
        )
        if not any("more than five minutes in the future" in error for error in errors):
            print(errors)
            print("SELF-TEST FAIL: future-dated performance evidence must fail closed")
            return 2

        dirty_root, _, dirty_source = create_binding_repo(base, "binding-dirty")
        (dirty_root / "Cargo.toml").write_text(
            (dirty_root / "Cargo.toml").read_text(encoding="utf-8") + "# dirty\n",
            encoding="utf-8",
        )
        binding_error = performance_source_binding_error(
            dirty_root,
            dirty_source,
            "tests/perf/reports/budget_summary.json",
        )
        if binding_error is None or "repository is not clean" not in binding_error:
            print(binding_error)
            print("SELF-TEST FAIL: unstaged tracked dirt must invalidate source binding")
            return 2

        staged_root, _, staged_source = create_binding_repo(base, "binding-staged")
        (staged_root / "Cargo.toml").write_text(
            (staged_root / "Cargo.toml").read_text(encoding="utf-8") + "# staged\n",
            encoding="utf-8",
        )
        git(staged_root, "add", "Cargo.toml")
        binding_error = performance_source_binding_error(
            staged_root,
            staged_source,
            "tests/perf/reports/budget_summary.json",
        )
        if binding_error is None or "repository is not clean" not in binding_error:
            print(binding_error)
            print("SELF-TEST FAIL: staged dirt must invalidate source binding")
            return 2

        untracked_root, _, untracked_source = create_binding_repo(base, "binding-untracked")
        (untracked_root / "untracked.txt").write_text("untracked\n", encoding="utf-8")
        binding_error = performance_source_binding_error(
            untracked_root,
            untracked_source,
            "tests/perf/reports/budget_summary.json",
        )
        if binding_error is None or "repository is not clean" not in binding_error:
            print(binding_error)
            print("SELF-TEST FAIL: untracked dirt must invalidate source binding")
            return 2

        head_root, _, _ = create_binding_repo(base, "binding-head-dirty")
        head_commit = git(head_root, "rev-parse", "HEAD").decode("ascii").strip()
        (head_root / "untracked.txt").write_text("untracked\n", encoding="utf-8")
        binding_error = performance_source_binding_error(
            head_root,
            head_commit,
            "tests/perf/reports/budget_summary.json",
        )
        if binding_error is None or "repository is not clean" not in binding_error:
            print(binding_error)
            print("SELF-TEST FAIL: source_commit=HEAD must not bypass cleanliness checks")
            return 2

        flags_root, _, flags_source = create_binding_repo(base, "binding-index-flags")
        git(flags_root, "update-index", "--assume-unchanged", "Cargo.toml")
        binding_error = performance_source_binding_error(
            flags_root,
            flags_source,
            "tests/perf/reports/budget_summary.json",
        )
        if binding_error is None or "non-default" not in binding_error:
            print(binding_error)
            print("SELF-TEST FAIL: hidden default-index flags must invalidate binding")
            return 2

        if os.name != "nt":
            mode_root, _, mode_source = create_binding_repo(
                base, "binding-hidden-mode"
            )
            mode_artifact = mode_root / "tests/perf/reports/budget_summary.json"
            git(mode_root, "config", "core.filemode", "false")
            os.chmod(mode_artifact, mode_artifact.stat().st_mode | stat.S_IXUSR)
            if git(mode_root, "status", "--porcelain"):
                print("SELF-TEST FAIL: core.filemode fixture did not hide mode drift")
                return 2
            binding_error = performance_source_binding_error(
                mode_root,
                mode_source,
                "tests/perf/reports/budget_summary.json",
            )
            if binding_error is None or "executable mode" not in binding_error:
                print(binding_error)
                print("SELF-TEST FAIL: hidden executable mode drift must invalidate binding")
                return 2

        hidden_root, _, hidden_source = create_binding_repo(base, "binding-hidden-index")
        original_cargo = (hidden_root / "Cargo.toml").read_text(encoding="utf-8")
        (hidden_root / "Cargo.toml").write_text(original_cargo + "# staged default index\n", encoding="utf-8")
        git(hidden_root, "add", "Cargo.toml")
        (hidden_root / "Cargo.toml").write_text(original_cargo, encoding="utf-8")
        alternate_index = base / "alternate.index"
        alternate_env = os.environ.copy()
        alternate_env["GIT_INDEX_FILE"] = str(alternate_index)
        git(hidden_root, "read-tree", "HEAD", env=alternate_env)
        previous_index = os.environ.get("GIT_INDEX_FILE")
        os.environ["GIT_INDEX_FILE"] = str(alternate_index)
        try:
            binding_error = performance_source_binding_error(
                hidden_root,
                hidden_source,
                "tests/perf/reports/budget_summary.json",
            )
        finally:
            if previous_index is None:
                os.environ.pop("GIT_INDEX_FILE", None)
            else:
                os.environ["GIT_INDEX_FILE"] = previous_index
        if binding_error is None or "repository is not clean" not in binding_error:
            print(binding_error)
            print("SELF-TEST FAIL: alternate GIT_INDEX_FILE must not hide default-index dirt")
            return 2

        hostile_config = {
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "core.worktree",
            "GIT_CONFIG_VALUE_0": str(base / "hostile-config-worktree"),
        }
        previous_config = {key: os.environ.get(key) for key in hostile_config}
        os.environ.update(hostile_config)
        try:
            binding_error = performance_source_binding_error(
                valid_root,
                source_commit,
                "tests/perf/reports/budget_summary.json",
            )
        finally:
            for key, previous in previous_config.items():
                if previous is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = previous
        if binding_error is not None:
            print(binding_error)
            print("SELF-TEST FAIL: hostile Git config injection must be ignored")
            return 2

        followup_root, _, followup_source = create_binding_repo(base, "binding-followup")
        (followup_root / "non_evidence.txt").write_text("product change\n", encoding="utf-8")
        git(followup_root, "add", "non_evidence.txt")
        git(followup_root, "commit", "-q", "-m", "non-evidence followup")
        binding_error = performance_source_binding_error(
            followup_root,
            followup_source,
            "tests/perf/reports/budget_summary.json",
        )
        if binding_error is None or "non-evidence path changed" not in binding_error:
            print(binding_error)
            print("SELF-TEST FAIL: non-evidence source followups must invalidate binding")
            return 2

        default_package_root, _, default_package_source = create_binding_repo(
            base,
            "binding-default-package-policy",
            package_include=None,
        )
        default_evidence = default_package_root / "docs/evidence/unproved-policy.json"
        default_evidence.parent.mkdir(parents=True)
        default_evidence.write_text("{}\n", encoding="utf-8")
        git(default_package_root, "add", "docs/evidence/unproved-policy.json")
        git(default_package_root, "commit", "-q", "-m", "unproved evidence followup")
        binding_error = performance_source_binding_error(
            default_package_root,
            default_package_source,
            "tests/perf/reports/budget_summary.json",
        )
        if binding_error is None or "package.include must be an array" not in binding_error:
            print(binding_error)
            print(
                "SELF-TEST FAIL: absent package.include must not authorize docs/evidence followups"
            )
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
