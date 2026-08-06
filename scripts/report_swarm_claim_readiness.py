#!/usr/bin/env python3
"""Report swarm-scale evidence freshness and claim readiness.

Normal report generation is read-only. Without --gate it exits 0 and prints the
current evidence state for operators. With --gate it exits 1 when release-facing
claim evidence is missing, stale, no-data, invalid, or provenance-mismatched.
The --self-test mode writes disposable fixture directories under TMPDIR and does
not delete them.
"""

from __future__ import annotations

import argparse
import contextlib
import difflib
import fnmatch
import json
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable

from check_readme_evidence_freshness import (
    ReleaseArtifactSnapshot,
    capture_release_artifact_snapshot,
    performance_budget_claim_errors,
    revalidate_release_artifact_snapshot,
)


REPORT_SCHEMA = "pi.swarm.claim_readiness_report.v1"
PERF_BUDGET_SUMMARY_SCHEMA = "pi.perf.budget_summary.v2"
STALE_CLAIM_REPORT_SCHEMA = "pi.swarm.stale_claim_report.v1"
REDUNDANT_AGENT_WORK_SCHEMA = "pi.swarm.redundant_agent_work.v1"
HOSTCALL_QUEUE_REPORT_SCHEMA = "pi.swarm.hostcall_queue_readiness.v1"
OPERATOR_EXPLANATIONS_SCHEMA = "pi.swarm.operator_explanations.v1"
GOLDEN_REPORT_DIRECTORY = Path("tests/golden_corpus/swarm_claim_readiness")
COMPLETE_REPORT_GOLDEN = "complete_report_projection.json"
UPDATE_GOLDEN_ENV = "UPDATE_SWARM_CLAIM_READINESS_GOLDEN"
_FIXTURE_GIT_TEMPLATE: Path | None = None
DEFAULT_MAX_AGE_DAYS = 14
DEFAULT_STALE_CLAIM_AFTER_HOURS = 24
DEFAULT_STALE_CLAIM_ACTIVITY_FRESH_HOURS = 6
DEFAULT_STALE_CLAIM_ACTIVITY_PATHS = (
    "tests/full_suite_gate/swarm_activity_events.jsonl",
    "tests/full_suite_gate/swarm_activity_ledger.jsonl",
)
MAX_FUTURE_CLOCK_SKEW = timedelta(minutes=5)
BEAD_ID_PATTERN = re.compile(r"\bbd-[A-Za-z0-9][A-Za-z0-9.-]*\b")
BACKTICK_PATH_PATTERN = re.compile(r"`([^`]+)`")
BARE_REPO_PATH_PATTERN = re.compile(
    r"\b(?:src|tests|scripts|docs|crates|examples|benches|\.beads)/"
    r"[A-Za-z0-9_./*{}\\[\\]-]+"
)
REPO_PATH_SUFFIXES = (
    ".json",
    ".jsonl",
    ".md",
    ".py",
    ".rs",
    ".sh",
    ".toml",
    ".yaml",
    ".yml",
)

DEFAULT_TIMESTAMP_PATHS = (
    "generated_at",
    "generated_at_utc",
    "effective_date_utc",
    "summary.generated_at",
)
DEFAULT_PROVENANCE_PATHS = (
    "correlation_id",
    "run_id",
    "ci_run_id",
    "git_commit",
    "git_ref",
    "bead_id",
)
TITLE_TOKEN_STOPWORDS = {
    "a",
    "add",
    "and",
    "for",
    "in",
    "of",
    "the",
    "to",
    "with",
}


@dataclass(frozen=True)
class EvidenceSpec:
    id: str
    category: str
    path: str
    description: str
    claim_surface: str
    release_blocking: bool = True
    generated: bool = True
    required_schema: str | None = None
    timestamp_paths: tuple[str, ...] = DEFAULT_TIMESTAMP_PATHS
    status_path: str | None = None
    ok_values: tuple[Any, ...] = ()
    zero_paths: tuple[str, ...] = ()
    required_true_paths: tuple[str, ...] = ()
    required_empty_list_paths: tuple[str, ...] = ()
    provenance_paths: tuple[str, ...] = DEFAULT_PROVENANCE_PATHS
    provenance_group: str | None = None


@dataclass(frozen=True)
class EvidenceIssue:
    kind: str
    detail: str
    blocking: bool

    def to_json(self) -> dict[str, Any]:
        return {
            "kind": self.kind,
            "detail": self.detail,
            "blocking": self.blocking,
        }


@dataclass
class EvidenceCheck:
    spec: EvidenceSpec
    exists: bool
    generated_at: datetime | None
    age_days: float | None
    schema: str | None
    provenance_value: str | None
    issues: list[EvidenceIssue]
    release_snapshot: ReleaseArtifactSnapshot | None = None
    parsed_payload: dict[str, Any] | None = None

    def status(self) -> str:
        kinds = {issue.kind for issue in self.issues}
        for kind in (
            "missing",
            "invalid_json",
            "schema_mismatch",
            "status_not_ready",
            "no_data",
            "missing_timestamp",
            "future_timestamp",
            "stale",
            "provenance_mismatch",
            "source_binding",
        ):
            if kind in kinds:
                return kind
        if self.spec.claim_surface == "historical_snapshot":
            return "historical_snapshot"
        if self.spec.claim_surface == "release_policy":
            return "release_policy"
        return "ready"

    def blocking_issue_count(self) -> int:
        return sum(1 for issue in self.issues if issue.blocking)

    def to_json(self) -> dict[str, Any]:
        return {
            "id": self.spec.id,
            "category": self.spec.category,
            "path": self.spec.path,
            "description": self.spec.description,
            "claim_surface": self.spec.claim_surface,
            "release_blocking": self.spec.release_blocking,
            "status": self.status(),
            "exists": self.exists,
            "schema": self.schema,
            "generated_at": format_datetime(self.generated_at),
            "age_days": round(self.age_days, 2) if self.age_days is not None else None,
            "provenance_value": self.provenance_value,
            "issues": [issue.to_json() for issue in self.issues],
        }


@dataclass(frozen=True)
class ClaimActivityEvidence:
    bead_id: str
    timestamp: datetime
    source: str
    agent_name: str | None


@dataclass(frozen=True)
class HostcallQueueSourceSpec:
    id: str
    path: str
    reactor_paths: tuple[str, ...]


@dataclass(frozen=True)
class HostcallQueueMetricSpec:
    name: str
    paths: tuple[str, ...]
    required: bool = True


@dataclass(frozen=True)
class ReadmeSectionSpec:
    id: str
    start_marker: str
    end_marker: str


@dataclass(frozen=True)
class ReadmeLineCheck:
    id: str
    section_id: str
    anchor: str
    expected: str
    source_path: str
    description: str


HOSTCALL_QUEUE_SOURCES = (
    HostcallQueueSourceSpec(
        id="perf_stress_triage",
        path="tests/perf/reports/stress_triage.json",
        reactor_paths=("results.reactor", "reactor"),
    ),
    HostcallQueueSourceSpec(
        id="extension_reactor_queue_coverage",
        path="docs/evidence/ext-stress-reactor-queue-coverage.json",
        reactor_paths=("captured_report_metrics.reactor", "results.reactor", "reactor"),
    ),
)

README_SECTIONS = (
    ReadmeSectionSpec(
        id="latest_run_snapshot",
        start_marker="### Latest run snapshot",
        end_marker="## Installation",
    ),
    ReadmeSectionSpec(
        id="certification_evidence_refresh",
        start_marker="Latest certification/evidence refresh",
        end_marker="### Fast Loop",
    ),
)
README_SNAPSHOT_JSON_PATH_RE = re.compile(
    r"`?((?:docs|tests)/[A-Za-z0-9_./-]+\.json)`?"
)
README_SNAPSHOT_GENERATED_RE = re.compile(
    r"\s+\(generated `([^`]+)`(?:, run `([^`]+)`)?\)"
)

HOSTCALL_QUEUE_METRICS = (
    HostcallQueueMetricSpec(
        name="s3fifo_mode",
        paths=("s3fifo.mode", "s3fifo_mode"),
        required=False,
    ),
    HostcallQueueMetricSpec(
        name="s3fifo_fallback_reason",
        paths=("s3fifo.fallback_reason", "s3fifo_fallback_reason"),
        required=False,
    ),
    HostcallQueueMetricSpec(
        name="s3fifo_fallback_transitions",
        paths=(
            "s3fifo.fallback_event_total",
            "s3fifo.fallback_transitions",
            "s3fifo_fallback_transitions",
        ),
    ),
    HostcallQueueMetricSpec(
        name="s3fifo_fairness_rejected_total",
        paths=(
            "s3fifo.fairness_budget_rejections",
            "s3fifo.fairness_rejected_total",
            "s3fifo_fairness_rejected_total",
            "stall_reasons.fairness_budget",
        ),
    ),
    HostcallQueueMetricSpec(
        name="s3fifo_lane_overflow_rejected_total",
        paths=(
            "s3fifo.lane_overflow_rejections",
            "stall_reasons.lane_overflow",
            "rejected_enqueues",
            "overflow_rejected_total",
        ),
    ),
    HostcallQueueMetricSpec(
        name="s3fifo_ghost_hits_total",
        paths=("s3fifo.ghost_hits_total", "s3fifo_ghost_hits_total"),
        required=False,
    ),
    HostcallQueueMetricSpec(
        name="queue_overflow_rejected_total",
        paths=("overflow_rejected_total", "rejected_enqueues"),
        required=False,
    ),
    HostcallQueueMetricSpec(
        name="safe_reclamation_fallback_transitions",
        paths=("fallback_transitions", "safe_reclamation_fallback_transitions"),
        required=False,
    ),
    HostcallQueueMetricSpec(
        name="bravo_mode",
        paths=("bravo.mode", "bravo_mode"),
    ),
    HostcallQueueMetricSpec(
        name="bravo_transitions_total",
        paths=("bravo.transitions", "bravo_transitions"),
    ),
    HostcallQueueMetricSpec(
        name="bravo_rollbacks_total",
        paths=("bravo.rollbacks", "bravo_rollbacks"),
    ),
    HostcallQueueMetricSpec(
        name="bravo_writer_recovery_remaining",
        paths=("bravo.writer_recovery_remaining", "bravo_writer_recovery_remaining"),
        required=False,
    ),
    HostcallQueueMetricSpec(
        name="bravo_last_signature",
        paths=("bravo.last_signature", "bravo_last_signature"),
        required=False,
    ),
)


EVIDENCE_SPECS = (
    EvidenceSpec(
        id="perf_budget_summary",
        category="perf",
        path="tests/perf/reports/budget_summary.json",
        description="CI-enforced performance budget summary.",
        claim_surface="release_facing",
        required_schema=PERF_BUDGET_SUMMARY_SCHEMA,
        status_path="claim_readiness.status",
        ok_values=("claim_ready",),
        zero_paths=(
            "fail",
            "no_data",
            "ci_fail",
            "ci_no_data",
            "data_contract_failures_count",
        ),
        required_true_paths=(
            "strict_mode",
            "claim_readiness.performance_claims_authorized",
        ),
        required_empty_list_paths=("claim_readiness.blocking_reason_codes",),
        provenance_group="perf",
    ),
    EvidenceSpec(
        id="perf_stress_triage",
        category="perf",
        path="tests/perf/reports/stress_triage.json",
        description="Extension stress and reactor queue performance triage.",
        claim_surface="release_facing",
        status_path="pass",
        ok_values=(True,),
        provenance_group="perf",
    ),
    EvidenceSpec(
        id="perf3x_bead_coverage",
        category="perf",
        path="tests/full_suite_gate/perf3x_bead_coverage_audit.json",
        description="Perf evidence bead coverage audit.",
        claim_surface="release_facing",
        required_schema="pi.perf3x.bead_coverage.audit.v1",
        status_path="status",
        ok_values=("pass",),
        zero_paths=("missing_evidence_count",),
        provenance_group="full_suite",
    ),
    EvidenceSpec(
        id="full_suite_verdict",
        category="full_suite",
        path="tests/full_suite_gate/full_suite_verdict.json",
        description="Full-suite gate verdict.",
        claim_surface="release_facing",
        required_schema="pi.ci.full_suite_gate.v1",
        status_path="verdict",
        ok_values=("pass",),
        provenance_group="full_suite",
    ),
    EvidenceSpec(
        id="certification_lane",
        category="full_suite",
        path="tests/full_suite_gate/certification_verdict.json",
        description="Certification lane verdict.",
        claim_surface="release_facing",
        required_schema="pi.ci.certification_lane.v1",
        status_path="verdict",
        ok_values=("pass",),
        provenance_group="full_suite",
    ),
    EvidenceSpec(
        id="evidence_bundle_index",
        category="full_suite",
        path="tests/evidence_bundle/index.json",
        description="Indexed evidence bundle for release triage.",
        claim_surface="release_facing",
        required_schema="pi.ci.evidence_bundle.v1",
        provenance_group="full_suite",
    ),
    EvidenceSpec(
        id="dropin_certification_verdict",
        category="dropin",
        path="docs/evidence/dropin-certification-verdict.json",
        description="Strict drop-in release claim gate.",
        claim_surface="release_facing",
        required_schema="pi.dropin.certification_verdict.v1",
        status_path="overall_verdict",
        ok_values=("CERTIFIED",),
        provenance_group="dropin",
    ),
    EvidenceSpec(
        id="dropin_contract",
        category="dropin",
        path="docs/contracts/dropin-certification-contract.json",
        description="Normative drop-in claim policy contract.",
        claim_surface="release_policy",
        generated=False,
        required_schema="pi.dropin.certification_contract.v1",
    ),
    EvidenceSpec(
        id="dropin_differential_suite",
        category="dropin",
        path="docs/evidence/dropin-differential-evidence-suite.json",
        description="Differential evidence suite backing drop-in claims.",
        claim_surface="historical_snapshot",
        required_schema="pi.dropin.differential_evidence_suite.v1",
        generated=False,
        timestamp_paths=(),
        provenance_paths=(),
    ),
    EvidenceSpec(
        id="dropin_feature_inventory",
        category="dropin",
        path="docs/evidence/dropin-feature-inventory-matrix.json",
        description="Feature inventory matrix for drop-in parity.",
        claim_surface="historical_snapshot",
        required_schema="pi.dropin.feature_inventory.v1",
        generated=False,
        timestamp_paths=(),
        provenance_paths=(),
    ),
    EvidenceSpec(
        id="dropin_gap_ledger",
        category="dropin",
        path="docs/evidence/dropin-parity-gap-ledger.json",
        description="Parity gap ledger used by release claim policy.",
        claim_surface="historical_snapshot",
        required_schema="pi.dropin.parity_gap_ledger.v1",
        generated=False,
        timestamp_paths=(),
        provenance_paths=(),
    ),
    EvidenceSpec(
        id="extension_must_pass_gate",
        category="extension",
        path="tests/ext_conformance/reports/gate/must_pass_gate_verdict.json",
        description="Strict extension must-pass verdict.",
        claim_surface="release_facing",
        required_schema="pi.ext.must_pass_gate.v1",
        status_path="status",
        ok_values=("pass",),
        zero_paths=("observed.must_pass_failed", "observed.must_pass_skipped"),
        provenance_group="extension_conformance",
    ),
    EvidenceSpec(
        id="extension_health_delta",
        category="extension",
        path="tests/ext_conformance/reports/health_delta/health_delta_report.json",
        description="Extension health delta against baseline.",
        claim_surface="release_facing",
        required_schema="pi.ext.health_delta.v1",
        zero_paths=("current_summary.skipped",),
        provenance_group="extension_conformance",
    ),
    EvidenceSpec(
        id="extension_journey_report",
        category="extension",
        path="tests/ext_conformance/reports/journeys/journey_report.json",
        description="Historical extension journey and remaining stretch failures.",
        claim_surface="historical_snapshot",
        generated=False,
    ),
    EvidenceSpec(
        id="extension_reactor_queue_coverage",
        category="extension",
        path="docs/evidence/ext-stress-reactor-queue-coverage.json",
        description="Reactor queue stress coverage evidence.",
        claim_surface="release_facing",
        required_schema="pi.ext.reactor_queue_coverage_evidence.v1",
        status_path="status",
        ok_values=("PASS", "pass"),
        provenance_group="extension_reactor_queue",
    ),
    EvidenceSpec(
        id="activity_ledger_docs",
        category="activity_ledger",
        path="docs/swarm-activity-ledger.md",
        description="Operator contract for swarm activity ledger rows and digests.",
        claim_surface="historical_snapshot",
        generated=False,
    ),
    EvidenceSpec(
        id="activity_ledger_digest",
        category="activity_ledger",
        path="tests/full_suite_gate/swarm_activity_digest.json",
        description="Generated bounded digest for the latest multi-agent run.",
        claim_surface="release_facing",
        required_schema="pi.swarm.activity_digest.v1",
        provenance_group="activity_ledger",
    ),
)


def as_utc(value: datetime) -> datetime:
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def parse_iso_datetime(raw: Any) -> datetime | None:
    if not isinstance(raw, str):
        return None
    value = raw.strip()
    if not value:
        return None
    if value.endswith("Z"):
        value = f"{value[:-1]}+00:00"
    if "." in value:
        prefix, suffix = value.split(".", 1)
        digits = []
        rest_start = 0
        for idx, char in enumerate(suffix):
            if char.isdigit():
                digits.append(char)
                rest_start = idx + 1
            else:
                break
        if digits:
            fraction = "".join(digits)[:6].ljust(6, "0")
            value = f"{prefix}.{fraction}{suffix[rest_start:]}"
    try:
        return as_utc(datetime.fromisoformat(value))
    except ValueError:
        return None


def format_datetime(value: datetime | None) -> str | None:
    if value is None:
        return None
    return value.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def get_path(payload: Any, path: str) -> Any:
    current = payload
    for part in path.split("."):
        if isinstance(current, dict) and part in current:
            current = current[part]
        else:
            return None
    return current


def first_path(payload: Any, paths: tuple[str, ...]) -> tuple[str | None, Any]:
    for path in paths:
        value = get_path(payload, path)
        if value is not None:
            return path, value
    return None, None


def normalize_value(value: Any) -> Any:
    if isinstance(value, str):
        return value.strip()
    return value


def is_zero(value: Any) -> bool:
    if value is False:
        return True
    if value is True:
        return False
    if isinstance(value, (int, float)):
        return value == 0
    if isinstance(value, str):
        stripped = value.strip().lower()
        if stripped in {"", "0", "0.0", "false", "none", "null"}:
            return True
        try:
            return float(stripped) == 0.0
        except ValueError:
            return False
    if isinstance(value, (list, tuple, dict, set)):
        return len(value) == 0
    return value is None


def non_negative_count(value: Any) -> int | None:
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, int):
        return value if value >= 0 else None
    if isinstance(value, float):
        return int(value) if value >= 0 and value.is_integer() else None
    if isinstance(value, str):
        stripped = value.strip()
        if stripped.isdigit():
            return int(stripped)
    return None


def zero_path_issue_detail(
    spec: EvidenceSpec,
    payload: dict[str, Any],
    zero_path: str,
) -> str | None:
    value = get_path(payload, zero_path)
    if value is None:
        return f"{zero_path} is missing"

    if spec.id == "extension_health_delta" and zero_path == "current_summary.skipped":
        skipped_count = non_negative_count(value)
        excluded_value = get_path(payload, "current_summary.excluded")
        excluded_count = non_negative_count(excluded_value)
        if skipped_count is not None and excluded_count is not None:
            if excluded_count > skipped_count:
                return (
                    f"current_summary.excluded={excluded_value!r} exceeds "
                    f"{zero_path}={value!r}"
                )
            unaccounted_skips = skipped_count - excluded_count
            if unaccounted_skips == 0:
                return None
            return (
                f"{zero_path}={value!r} with "
                f"current_summary.excluded={excluded_value!r} leaves "
                f"unaccounted_skipped={unaccounted_skips}"
            )

    if not is_zero(value):
        return f"{zero_path}={value!r}"
    return None


def issue_for(spec: EvidenceSpec, kind: str, detail: str) -> EvidenceIssue:
    blocking = spec.release_blocking and spec.claim_surface == "release_facing"
    return EvidenceIssue(kind=kind, detail=detail, blocking=blocking)


def reject_duplicate_json_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    payload: dict[str, Any] = {}
    for key, value in pairs:
        if key in payload:
            raise ValueError(f"duplicate JSON object key: {key}")
        payload[key] = value
    return payload


def load_json_bytes(contents: bytes) -> tuple[dict[str, Any] | None, str | None]:
    """Decode one already captured JSON byte snapshot without duplicate keys."""
    try:
        text = contents.decode("utf-8", "strict")
        payload = json.loads(text, object_pairs_hook=reject_duplicate_json_keys)
    except (json.JSONDecodeError, ValueError) as exc:
        return None, f"invalid JSON: {exc}"
    except UnicodeDecodeError:
        return None, "artifact is not UTF-8"
    if not isinstance(payload, dict):
        return None, "JSON artifact must be an object"
    return payload, None


def load_json(path: Path) -> tuple[dict[str, Any] | None, str | None]:
    try:
        contents = path.read_bytes()
    except OSError as exc:
        return None, f"failed to read artifact: {exc}"
    return load_json_bytes(contents)


def parse_json_metadata_string(
    raw: str,
) -> tuple[dict[str, Any] | None, str | None]:
    """Parse optional JSON-encoded metadata and reject recursive duplicate keys."""
    try:
        payload = json.loads(raw, object_pairs_hook=reject_duplicate_json_keys)
    except json.JSONDecodeError:
        return None, None
    except ValueError as exc:
        return None, str(exc)
    return (payload if isinstance(payload, dict) else None), None


def load_jsonl_objects(path: Path) -> tuple[list[dict[str, Any]], str | None]:
    objects: list[dict[str, Any]] = []
    try:
        with path.open(encoding="utf-8") as handle:
            for line_number, line in enumerate(handle, start=1):
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    payload = json.loads(
                        stripped,
                        object_pairs_hook=reject_duplicate_json_keys,
                    )
                except (json.JSONDecodeError, ValueError) as exc:
                    return objects, f"{path}:{line_number}: invalid JSON: {exc}"
                if isinstance(payload, dict):
                    metadata = payload.get("metadata")
                    if isinstance(metadata, str) and metadata.strip():
                        _, metadata_error = parse_json_metadata_string(metadata)
                        if metadata_error is not None:
                            return (
                                objects,
                                f"{path}:{line_number}: invalid metadata JSON: "
                                f"{metadata_error}",
                            )
                    payload["_source_line"] = line_number
                    objects.append(payload)
    except UnicodeDecodeError:
        return objects, f"{path}: artifact is not UTF-8"
    except OSError as exc:
        return objects, f"{path}: failed to read artifact: {exc}"
    return objects, None


def parse_epoch_millis(raw: Any) -> datetime | None:
    if isinstance(raw, bool) or raw is None:
        return None
    if isinstance(raw, (int, float)):
        try:
            return datetime.fromtimestamp(float(raw) / 1000.0, tz=timezone.utc)
        except (OSError, OverflowError, ValueError):
            return None
    if isinstance(raw, str):
        stripped = raw.strip()
        if stripped.isdigit():
            try:
                return datetime.fromtimestamp(int(stripped) / 1000.0, tz=timezone.utc)
            except (OSError, OverflowError, ValueError):
                return None
    return None


def first_timestamp(payload: dict[str, Any]) -> datetime | None:
    timestamp_ms = parse_epoch_millis(get_path(payload, "timestamp_ms"))
    if timestamp_ms is not None:
        return timestamp_ms
    _, raw = first_path(
        payload,
        (
            "timestamp",
            "created_ts",
            "created_at",
            "updated_at",
            "details.timestamp",
            "details.created_ts",
            "details.updated_at",
        ),
    )
    return parse_iso_datetime(raw)


def collect_claim_activity(
    repo_root: Path,
    activity_paths: tuple[str, ...],
) -> tuple[dict[str, ClaimActivityEvidence], list[str], list[str]]:
    latest: dict[str, ClaimActivityEvidence] = {}
    used_paths: list[str] = []
    warnings: list[str] = []
    for relative_path in activity_paths:
        full_path = repo_root / relative_path
        if not full_path.exists():
            continue
        used_paths.append(relative_path)
        rows, error = load_jsonl_objects(full_path)
        if error is not None:
            warnings.append(error)
        for row in rows:
            bead_id = get_path(row, "ids.bead_id") or row.get("bead_id")
            if not isinstance(bead_id, str) or not bead_id.strip():
                continue
            timestamp = first_timestamp(row)
            if timestamp is None:
                continue
            agent_name = get_path(row, "ids.agent_name") or row.get("agent_name")
            if not isinstance(agent_name, str):
                agent_name = None
            line_number = row.get("_source_line", "?")
            kind = row.get("kind") or row.get("phase") or "activity"
            source = f"{relative_path}:{line_number}:{kind}"
            evidence = ClaimActivityEvidence(
                bead_id=bead_id,
                timestamp=timestamp,
                source=source,
                agent_name=agent_name,
            )
            current = latest.get(bead_id)
            if current is None or evidence.timestamp > current.timestamp:
                latest[bead_id] = evidence
    return latest, used_paths, warnings


def git_output(repo_root: Path, args: list[str]) -> str | None:
    try:
        result = subprocess.run(
            ["git", "-C", str(repo_root), *args],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return result.stdout


def collect_dirty_worktree_paths(repo_root: Path) -> tuple[str, ...]:
    output = git_output(repo_root, ["status", "--porcelain=v1"])
    if output is None:
        return ()

    paths: list[str] = []
    for line in output.splitlines():
        if len(line) < 4:
            continue
        path = line[3:].strip()
        if " -> " in path:
            path = path.rsplit(" -> ", 1)[1]
        if path:
            paths.append(path)
    return tuple(sorted(set(paths)))


def collect_recent_commit_activity(
    repo_root: Path,
    *,
    now: datetime,
    fresh_after_hours: int,
) -> dict[str, ClaimActivityEvidence]:
    if fresh_after_hours <= 0:
        return {}
    since = now - timedelta(hours=fresh_after_hours)
    output = git_output(
        repo_root,
        [
            "log",
            f"--since={format_datetime(since)}",
            "--format=%H%x1f%cI%x1f%B%x1e",
            "-n",
            "200",
        ],
    )
    if output is None:
        return {}

    latest: dict[str, ClaimActivityEvidence] = {}
    for raw_record in output.split("\x1e"):
        record = raw_record.strip()
        if not record:
            continue
        parts = record.split("\x1f", 2)
        if len(parts) != 3:
            continue
        sha, raw_timestamp, message = parts
        timestamp = parse_iso_datetime(raw_timestamp)
        if timestamp is None:
            continue
        for bead_id in sorted(set(BEAD_ID_PATTERN.findall(message))):
            evidence = ClaimActivityEvidence(
                bead_id=bead_id,
                timestamp=timestamp,
                source=f"git log:{sha[:12]}",
                agent_name=None,
            )
            current = latest.get(bead_id)
            if current is None or evidence.timestamp > current.timestamp:
                latest[bead_id] = evidence
    return latest


def read_beads_records(repo_root: Path) -> tuple[list[dict[str, Any]], str | None, str]:
    relative_path = ".beads/issues.jsonl"
    rows, error = load_jsonl_objects(repo_root / relative_path)
    return rows, error, relative_path


def issue_assignee(issue: dict[str, Any]) -> str | None:
    raw = issue.get("assignee") or issue.get("assigned_to") or issue.get("owner")
    if isinstance(raw, str) and raw.strip():
        return raw.strip()
    return None


def as_string_list(value: Any) -> list[str]:
    if isinstance(value, str) and value.strip():
        return [value.strip()]
    if isinstance(value, list):
        return [item.strip() for item in value if isinstance(item, str) and item.strip()]
    return []


def looks_like_repo_path(value: str) -> bool:
    candidate = value.strip().strip(".,;:)")
    if not candidate or " " in candidate or candidate.startswith("-"):
        return False
    if "://" in candidate:
        return False
    if candidate.startswith(("br ", "git ", "cargo ")):
        return False
    return "/" in candidate or candidate.endswith(REPO_PATH_SUFFIXES)


def normalize_repo_path(value: str) -> str | None:
    candidate = value.strip().strip(".,;:)")
    while candidate.startswith("./"):
        candidate = candidate[2:]
    if candidate.startswith("/"):
        return None
    if not looks_like_repo_path(candidate):
        return None
    return candidate


def append_surface_path(paths: list[str], value: str) -> None:
    normalized = normalize_repo_path(value)
    if normalized is not None and normalized not in paths:
        paths.append(normalized)


def append_surface_paths_from_mapping(paths: list[str], payload: dict[str, Any]) -> None:
    for key in (
        "path",
        "paths",
        "file",
        "files",
        "file_path",
        "file_paths",
        "reservation_path",
        "reservation_paths",
        "touched_path",
        "touched_paths",
    ):
        for value in as_string_list(payload.get(key)):
            append_surface_path(paths, value)


def issue_surface_paths(issue: dict[str, Any]) -> tuple[str, ...]:
    paths: list[str] = []
    append_surface_paths_from_mapping(paths, issue)

    metadata = issue.get("metadata")
    if isinstance(metadata, str) and metadata.strip():
        parsed, _ = parse_json_metadata_string(metadata)
        if isinstance(parsed, dict):
            append_surface_paths_from_mapping(paths, parsed)
    elif isinstance(metadata, dict):
        append_surface_paths_from_mapping(paths, metadata)

    text_parts = []
    for key in ("title", "description"):
        value = issue.get(key)
        if isinstance(value, str):
            text_parts.append(value)
    comments = issue.get("comments")
    if isinstance(comments, list):
        for comment in comments:
            if isinstance(comment, dict):
                for key in ("text", "body", "body_md"):
                    value = comment.get(key)
                    if isinstance(value, str):
                        text_parts.append(value)
    text = "\n".join(text_parts)
    for match in BACKTICK_PATH_PATTERN.findall(text):
        append_surface_path(paths, match)
    for match in BARE_REPO_PATH_PATTERN.findall(text):
        append_surface_path(paths, match)

    return tuple(paths)


def dirty_path_matches_surface(dirty_path: str, surface_path: str) -> bool:
    dirty = dirty_path.strip().lstrip("./")
    surface = surface_path.strip().lstrip("./")
    if not dirty or not surface:
        return False
    if "*" in surface:
        return fnmatch.fnmatchcase(dirty, surface)
    surface_prefix = surface.rstrip("/")
    return dirty == surface_prefix or dirty.startswith(f"{surface_prefix}/")


def dirty_paths_for_issue(
    issue: dict[str, Any],
    dirty_paths: tuple[str, ...],
) -> tuple[tuple[str, ...], tuple[str, ...]]:
    surface_paths = issue_surface_paths(issue)
    if not surface_paths or not dirty_paths:
        return surface_paths, ()
    matches = [
        dirty_path
        for dirty_path in dirty_paths
        if any(dirty_path_matches_surface(dirty_path, surface) for surface in surface_paths)
    ]
    return surface_paths, tuple(sorted(set(matches)))


def safe_reopen_checklist(
    bead_id: str,
    assignee: str | None,
    surface_paths: tuple[str, ...],
) -> list[str]:
    owner_step = (
        f"Check Agent Mail thread {bead_id} and ask {assignee} for status; "
        "treat mail errors as degraded evidence, not abandonment."
        if assignee is not None
        else f"Check Agent Mail thread {bead_id}; no assignee is recorded in Beads."
    )
    status_step = (
        "Run `git status --short -- "
        + " ".join(surface_paths[:8])
        + "` and inspect dirty files on the bead surface."
        if surface_paths
        else "Run `git status --short` and inspect whether dirty files clearly belong to the bead."
    )
    return [
        f"Run `br show {bead_id}` and confirm status, assignee, dependencies, and updated_at.",
        owner_step,
        f"Run `git log --oneline --fixed-strings --grep={bead_id} -n 20` "
        "and review recent commits.",
        status_step,
        f"Only after those checks, run `br update {bead_id} --status open` if reopening is justified.",
    ]


def classify_stale_claim(
    issue: dict[str, Any],
    *,
    now: datetime,
    stale_after_hours: int,
    activity_fresh_after_hours: int,
    activity: ClaimActivityEvidence | None,
    recent_commit: ClaimActivityEvidence | None,
    dirty_worktree_paths: tuple[str, ...],
    bead_source: str,
) -> dict[str, Any]:
    bead_id = str(issue.get("id") or "")
    title = str(issue.get("title") or "")
    status = str(issue.get("status") or "")
    assignee = issue_assignee(issue)
    surface_paths, dirty_matches = dirty_paths_for_issue(issue, dirty_worktree_paths)
    updated_at_raw = issue.get("updated_at")
    updated_at = parse_iso_datetime(updated_at_raw)
    bead_age_hours: float | None = None
    latest_activity_age_hours: float | None = None
    latest_activity_at: str | None = None
    latest_commit_age_hours: float | None = None
    latest_commit_at: str | None = None
    evidence_source = bead_source
    reasons: list[str] = []
    future_timestamp_sources: list[str] = []

    if updated_at is not None:
        bead_age_hours = (now - updated_at).total_seconds() / 3600.0
        if updated_at > now + MAX_FUTURE_CLOCK_SKEW:
            future_timestamp_sources.append("bead.updated_at")
    else:
        reasons.append("bead updated_at is missing or unparseable")

    if activity is not None:
        latest_activity_at = format_datetime(activity.timestamp)
        latest_activity_age_hours = (
            now - activity.timestamp
        ).total_seconds() / 3600.0
        if activity.timestamp > now + MAX_FUTURE_CLOCK_SKEW:
            future_timestamp_sources.append("coordination_activity.timestamp")
        evidence_source = activity.source

    if recent_commit is not None:
        latest_commit_at = format_datetime(recent_commit.timestamp)
        latest_commit_age_hours = (
            now - recent_commit.timestamp
        ).total_seconds() / 3600.0
        if recent_commit.timestamp > now + MAX_FUTURE_CLOCK_SKEW:
            future_timestamp_sources.append("git_commit.timestamp")
        evidence_source = recent_commit.source

    if future_timestamp_sources:
        classification = "invalid_future_timestamp"
        operator_bucket = "ambiguous"
        reasons.append(
            "clock-integrity failure: materially future timestamp(s): "
            + ", ".join(future_timestamp_sources)
        )
        recommended_action = (
            f"Report-only: do not treat {bead_id} as active or stale until the future "
            "timestamp source is corrected and corroborated against Beads, Agent Mail, "
            "and git history."
        )
    elif updated_at is None:
        classification = "missing_evidence"
        operator_bucket = "ambiguous"
        recommended_action = (
            f"Report-only: do not reopen or reassign {bead_id}; inspect Beads and Agent Mail "
            "for owner evidence, then update the bead manually only after confirmation."
        )
    elif bead_age_hours < stale_after_hours:
        classification = "active"
        operator_bucket = "fresh"
        recommended_action = (
            f"No status change for {bead_id}; bead updated {bead_age_hours:.1f}h ago, "
            f"below stale threshold {stale_after_hours}h."
        )
    elif dirty_matches:
        classification = "active_dirty_worktree"
        operator_bucket = "fresh"
        evidence_source = "git status:dirty_worktree"
        reasons.append(
            "dirty worktree paths touch the bead surface: "
            + ", ".join(dirty_matches[:8])
        )
        recommended_action = (
            f"No reopen for {bead_id}; dirty worktree evidence touches the bead surface, "
            "so the claim may still be active."
        )
    elif (
        recent_commit is not None
        and latest_commit_age_hours is not None
        and latest_commit_age_hours < activity_fresh_after_hours
    ):
        classification = "active_recent_commit"
        operator_bucket = "fresh"
        reasons.append(
            f"recent commit evidence is {latest_commit_age_hours:.1f}h old, "
            f"below activity freshness threshold {activity_fresh_after_hours}h"
        )
        recommended_action = (
            f"No reopen for {bead_id}; recent git commit evidence from {recent_commit.source} "
            "indicates the claim is still active."
        )
    elif (
        activity is not None
        and latest_activity_age_hours is not None
        and latest_activity_age_hours < activity_fresh_after_hours
    ):
        classification = "active_recent_coordination"
        operator_bucket = "fresh"
        reasons.append(
            f"coordination activity is {latest_activity_age_hours:.1f}h old, "
            f"below activity freshness threshold {activity_fresh_after_hours}h"
        )
        recommended_action = (
            f"No reopen for {bead_id}; recent coordination evidence from {activity.source} "
            "indicates the claim is still active."
        )
    elif assignee is None:
        classification = "stale_candidate_unassigned"
        operator_bucket = "stale_candidate"
        reasons.append("in_progress bead has no assignee")
        recommended_action = (
            f"Report-only: consider `br update {bead_id} --status open` only after confirming "
            "there is no owner in Agent Mail or the activity ledger; no automatic change was made."
        )
    elif activity is None and recent_commit is None:
        classification = "ambiguous_missing_coordination"
        operator_bucket = "ambiguous"
        reasons.append(f"bead update age {bead_age_hours:.1f}h meets threshold {stale_after_hours}h")
        reasons.append(
            "optional Agent Mail/activity evidence is missing or degraded; absence is not proof "
            "the assignee is gone"
        )
        if not surface_paths:
            reasons.append("no explicit bead surface paths were found for dirty-file matching")
        recommended_action = (
            f"Report-only: message {assignee} in thread {bead_id} and inspect Beads, Agent Mail, "
            "recent commits, and dirty files before reopening. No automatic reopen or reassignment "
            "was performed."
        )
    else:
        classification = "stale_candidate_owner_follow_up"
        operator_bucket = "stale_candidate"
        reasons.append(f"bead update age {bead_age_hours:.1f}h meets threshold {stale_after_hours}h")
        if activity is not None:
            reasons.append(
                f"latest coordination activity age {latest_activity_age_hours:.1f}h "
                f"meets freshness threshold {activity_fresh_after_hours}h"
            )
        if recent_commit is not None:
            reasons.append(
                f"latest commit evidence age {latest_commit_age_hours:.1f}h "
                f"meets freshness threshold {activity_fresh_after_hours}h"
            )
        recommended_action = (
            f"Report-only: message {assignee} in thread {bead_id}; run "
            f"`br update {bead_id} --status open` only after confirming the owner is stale. "
            "No automatic reopen or reassignment was performed."
        )

    return {
        "bead_id": bead_id,
        "title": title,
        "status": status,
        "assignee": assignee,
        "last_update": format_datetime(updated_at),
        "bead_age_hours": round(bead_age_hours, 2) if bead_age_hours is not None else None,
        "latest_activity_at": latest_activity_at,
        "latest_activity_age_hours": (
            round(latest_activity_age_hours, 2)
            if latest_activity_age_hours is not None
            else None
        ),
        "latest_commit_at": latest_commit_at,
        "latest_commit_age_hours": (
            round(latest_commit_age_hours, 2)
            if latest_commit_age_hours is not None
            else None
        ),
        "evidence_source": evidence_source,
        "classification": classification,
        "operator_bucket": operator_bucket,
        "surface_paths": list(surface_paths),
        "dirty_worktree_paths": list(dirty_matches),
        "recommended_operator_action": recommended_action,
        "safe_reopen_checklist": safe_reopen_checklist(bead_id, assignee, surface_paths),
        "reasons": reasons,
    }


def build_stale_claim_report(
    repo_root: Path,
    *,
    now: datetime,
    stale_after_hours: int,
    activity_fresh_after_hours: int,
    activity_paths: tuple[str, ...],
    dirty_worktree_paths: tuple[str, ...] | None = None,
    recent_commit_activity: dict[str, ClaimActivityEvidence] | None = None,
) -> dict[str, Any]:
    rows, beads_error, beads_path = read_beads_records(repo_root)
    activity_by_bead, used_activity_paths, activity_warnings = collect_claim_activity(
        repo_root,
        activity_paths,
    )
    if dirty_worktree_paths is None:
        dirty_worktree_paths = collect_dirty_worktree_paths(repo_root)
    if recent_commit_activity is None:
        recent_commit_activity = collect_recent_commit_activity(
            repo_root,
            now=now,
            fresh_after_hours=activity_fresh_after_hours,
        )
    warnings = activity_warnings[:]
    if beads_error is not None:
        warnings.append(beads_error)

    items = [
        classify_stale_claim(
            issue,
            now=now,
            stale_after_hours=stale_after_hours,
            activity_fresh_after_hours=activity_fresh_after_hours,
            activity=activity_by_bead.get(str(issue.get("id") or "")),
            recent_commit=recent_commit_activity.get(str(issue.get("id") or "")),
            dirty_worktree_paths=dirty_worktree_paths,
            bead_source=beads_path,
        )
        for issue in rows
        if issue.get("status") == "in_progress"
    ]
    classifications: dict[str, int] = {}
    buckets: dict[str, int] = {}
    for item in items:
        classification = item["classification"]
        classifications[classification] = classifications.get(classification, 0) + 1
        bucket = item["operator_bucket"]
        buckets[bucket] = buckets.get(bucket, 0) + 1

    stale_count = buckets.get("stale_candidate", 0)
    ambiguous_count = buckets.get("ambiguous", 0)
    missing_evidence_count = classifications.get("missing_evidence", 0)
    status = "needs_coordination" if stale_count or ambiguous_count or warnings else "ready"
    return {
        "schema": STALE_CLAIM_REPORT_SCHEMA,
        "status": status,
        "policy": "report_only_no_auto_reopen_or_reassign",
        "thresholds": {
            "stale_after_hours": stale_after_hours,
            "activity_fresh_after_hours": activity_fresh_after_hours,
        },
        "source_paths": {
            "beads_ledger": beads_path,
            "activity_jsonl": used_activity_paths,
            "dirty_worktree": "git status --porcelain=v1",
            "recent_commits": f"git log --since={activity_fresh_after_hours}h",
        },
        "warnings": warnings,
        "summary": {
            "in_progress_count": len(items),
            "active_count": buckets.get("fresh", 0),
            "ambiguous_count": ambiguous_count,
            "stale_count": stale_count,
            "unassigned_count": classifications.get("stale_candidate_unassigned", 0),
            "missing_evidence_count": missing_evidence_count,
            "classifications": classifications,
            "operator_buckets": buckets,
        },
        "items": items,
    }


def issue_labels(issue: dict[str, Any]) -> tuple[str, ...]:
    labels = issue.get("labels")
    if not isinstance(labels, list):
        return ()
    return tuple(
        sorted({
            label.strip().lower()
            for label in labels
            if isinstance(label, str) and label.strip()
        })
    )


def issue_title_tokens(issue: dict[str, Any]) -> set[str]:
    raw_title = issue.get("title")
    if not isinstance(raw_title, str):
        return set()
    return {
        token
        for token in re.findall(r"[a-z0-9]+", raw_title.lower())
        if len(token) > 2 and token not in TITLE_TOKEN_STOPWORDS
    }


def title_similarity(left: dict[str, Any], right: dict[str, Any]) -> float:
    left_tokens = issue_title_tokens(left)
    right_tokens = issue_title_tokens(right)
    if not left_tokens or not right_tokens:
        return 0.0
    overlap = left_tokens & right_tokens
    union = left_tokens | right_tokens
    if not union:
        return 0.0
    return len(overlap) / len(union)


def issue_closed_or_updated_at(issue: dict[str, Any]) -> datetime | None:
    for key in ("closed_at", "updated_at", "created_at"):
        timestamp = parse_iso_datetime(issue.get(key))
        if timestamp is not None:
            return timestamp
    return None


def recent_closed_issue(issue: dict[str, Any], *, now: datetime, max_age_days: int) -> bool:
    if issue.get("status") != "closed":
        return False
    timestamp = issue_closed_or_updated_at(issue)
    if timestamp is None:
        return False
    age_days = max(0.0, (now - timestamp).total_seconds() / 86400.0)
    return age_days <= max_age_days


def surface_paths_overlap(
    left_paths: tuple[str, ...],
    right_paths: tuple[str, ...],
) -> tuple[str, ...]:
    overlaps: list[str] = []
    for left in left_paths:
        for right in right_paths:
            if left == right:
                overlap = left
            elif dirty_path_matches_surface(left, right) or dirty_path_matches_surface(right, left):
                overlap = f"{left}<->{right}"
            else:
                continue
            if overlap not in overlaps:
                overlaps.append(overlap)
    return tuple(overlaps)


def owner_state_for_issue(
    issue: dict[str, Any],
    stale_claim_items: dict[str, dict[str, Any]],
) -> str:
    bead_id = str(issue.get("id") or "")
    status = str(issue.get("status") or "")
    if status == "open":
        return "unclaimed_open"
    if status == "closed":
        return "recently_closed"
    stale_item = stale_claim_items.get(bead_id)
    if stale_item is not None:
        return str(stale_item.get("classification") or "in_progress_unknown")
    if status == "in_progress":
        return "in_progress_unknown"
    return status or "unknown"


def issue_member_summary(
    issue: dict[str, Any],
    *,
    stale_claim_items: dict[str, dict[str, Any]],
    dirty_worktree_paths: tuple[str, ...],
    path_activity: dict[str, list[str]],
) -> dict[str, Any]:
    surface_paths, dirty_matches = dirty_paths_for_issue(issue, dirty_worktree_paths)
    recent_activity = {
        path: path_activity[path]
        for path in surface_paths
        if path in path_activity
    }
    return {
        "bead_id": str(issue.get("id") or ""),
        "title": str(issue.get("title") or ""),
        "status": str(issue.get("status") or ""),
        "assignee": issue_assignee(issue),
        "updated_at": issue.get("updated_at"),
        "closed_at": issue.get("closed_at"),
        "owner_state": owner_state_for_issue(issue, stale_claim_items),
        "labels": list(issue_labels(issue)),
        "surface_paths": list(surface_paths),
        "dirty_worktree_paths": list(dirty_matches),
        "recent_path_activity": recent_activity,
    }


def safe_duplicate_work_action(
    risk: str,
    bead_ids: tuple[str, ...],
    shared_surfaces: tuple[str, ...],
    coordination_classification: str,
) -> str:
    bead_list = ", ".join(bead_ids)
    surface_text = " ".join(shared_surfaces[:6])
    status_step = (
        f"run `git status --short -- {surface_text}`"
        if surface_text
        else "run `git status --short`"
    )
    if risk == "already_closed_duplicate_request":
        return (
            f"Report-only: inspect {bead_list} with `br show` and "
            "`git log --oneline --fixed-strings --grep=<bead-id>` before starting work; "
            "if the request is already satisfied, close or retitle only after manual confirmation. "
            "Do not revert or delete another agent's work."
        )
    if risk == "stale_owner_overlap":
        return (
            f"Report-only: inspect {bead_list}, {status_step}, and contact the recorded owner; "
            f"Agent Mail state is {coordination_classification}, so use Beads comments/status as "
            "the soft lock if mail cannot be trusted. Do not reassign, revert, or delete automatically."
        )
    if risk == "dirty_surface_overlap":
        return (
            f"Report-only: inspect {bead_list} and {status_step}; dirty files already touch this "
            "surface, so pick a disjoint path or coordinate with the owner. Do not revert or delete "
            "the dirty worktree changes."
        )
    return (
        f"Report-only: inspect {bead_list}, {status_step}, and coordinate with assignees before "
        "claiming overlapping work. Do not automatically reassign, revert, or delete."
    )


def classify_agent_mail_health(payload: dict[str, Any] | None, source: str) -> dict[str, Any]:
    if payload is None:
        return {
            "source": source,
            "status": "not_provided",
            "health_level": None,
            "classification": "not_provided",
            "detail": "No Agent Mail health payload was supplied; duplicate-work detection uses Beads and git only.",
        }
    status = str(payload.get("status") or "")
    health_level = payload.get("health_level")
    recovery = payload.get("recovery")
    semantic = payload.get("semantic_readiness")
    recovery_mode = recovery.get("mode") if isinstance(recovery, dict) else None
    recovery_action = recovery.get("next_action") if isinstance(recovery, dict) else None
    recovery_bundle = recovery.get("bundle_path") if isinstance(recovery, dict) else None
    semantic_status = semantic.get("status") if isinstance(semantic, dict) else None
    semantic_detail = semantic.get("detail") if isinstance(semantic, dict) else None
    if status == "ok" and health_level not in {"red", "error"} and recovery_mode != "corrupt":
        classification = "available"
        detail = "Agent Mail health is available for coordination."
    else:
        classification = "degraded_agent_mail"
        detail_parts = [
            f"status={status or 'unknown'}",
            f"health_level={health_level or 'unknown'}",
            f"recovery={recovery_mode or 'unknown'}",
            f"semantic_readiness={semantic_status or 'unknown'}",
        ]
        if semantic_detail:
            detail_parts.append(f"semantic_detail={semantic_detail}")
        if recovery_action:
            detail_parts.append(f"recovery_action={recovery_action}")
        detail = f"Agent Mail health is degraded: {', '.join(detail_parts)}."
    return {
        "source": source,
        "status": status or "unknown",
        "health_level": health_level,
        "semantic_readiness": semantic_status,
        "semantic_readiness_detail": semantic_detail,
        "recovery_mode": recovery_mode,
        "recovery_action": recovery_action,
        "recovery_bundle_path": recovery_bundle,
        "database_url": payload.get("database_url"),
        "classification": classification,
        "detail": detail,
    }


def performance_budget_contract_issues(
    repo_root: Path,
    spec: EvidenceSpec,
    payload: dict[str, Any],
    now: datetime,
) -> list[EvidenceIssue]:
    """Apply the canonical v2 claim contract and hardened source binding."""
    return [
        issue_for(spec, "status_not_ready", detail)
        for detail in performance_budget_claim_errors(
            repo_root,
            payload,
            now,
            spec.path,
        )
    ]


def collect_git_path_activity(
    repo_root: Path,
    surface_paths: tuple[str, ...],
    *,
    max_paths: int = 32,
) -> dict[str, list[str]]:
    activity: dict[str, list[str]] = {}
    inspected = 0
    for path in sorted(set(surface_paths)):
        if inspected >= max_paths:
            break
        if "*" in path or "{" in path or "}" in path:
            continue
        inspected += 1
        output = git_output(repo_root, ["log", "--oneline", "--max-count=5", "--", path])
        if output is None:
            continue
        lines = [line.strip() for line in output.splitlines() if line.strip()]
        if lines:
            activity[path] = lines
    return activity


def redundant_group(
    *,
    risk: str,
    issues: tuple[dict[str, Any], ...],
    shared_surfaces: tuple[str, ...],
    match_reason: str,
    confidence: float,
    stale_claim_items: dict[str, dict[str, Any]],
    dirty_worktree_paths: tuple[str, ...],
    path_activity: dict[str, list[str]],
    coordination: dict[str, Any],
) -> dict[str, Any]:
    bead_ids = tuple(str(issue.get("id") or "") for issue in issues)
    return {
        "group_id": f"{risk}:{':'.join(bead_ids)}",
        "risk": risk,
        "confidence": round(confidence, 2),
        "match_reason": match_reason,
        "shared_surface_paths": list(shared_surfaces),
        "coordination_classification": coordination["classification"],
        "members": [
            issue_member_summary(
                issue,
                stale_claim_items=stale_claim_items,
                dirty_worktree_paths=dirty_worktree_paths,
                path_activity=path_activity,
            )
            for issue in issues
        ],
        "evidence": {
            "commands": [
                *(f"br show {bead_id}" for bead_id in bead_ids if bead_id),
                (
                    "git status --short -- " + " ".join(shared_surfaces[:8])
                    if shared_surfaces
                    else "git status --short"
                ),
                (
                    "git log --oneline -- " + " ".join(shared_surfaces[:4])
                    if shared_surfaces
                    else "git log --oneline --fixed-strings --grep=<bead-id>"
                ),
            ],
            "agent_mail": coordination["detail"],
        },
        "recommended_operator_action": safe_duplicate_work_action(
            risk,
            bead_ids,
            shared_surfaces,
            str(coordination["classification"]),
        ),
    }


def build_redundant_agent_work_report(
    repo_root: Path,
    *,
    now: datetime,
    stale_claims: dict[str, Any],
    agent_mail_health: dict[str, Any] | None = None,
    agent_mail_health_source: str = "not_provided",
    dirty_worktree_paths: tuple[str, ...] | None = None,
    path_activity: dict[str, list[str]] | None = None,
    recent_closed_after_days: int = DEFAULT_MAX_AGE_DAYS,
) -> dict[str, Any]:
    rows, beads_error, beads_path = read_beads_records(repo_root)
    warnings = [beads_error] if beads_error is not None else []
    dirty_paths = dirty_worktree_paths
    if dirty_paths is None:
        dirty_paths = collect_dirty_worktree_paths(repo_root)

    stale_claim_items = {
        str(item.get("bead_id") or ""): item
        for item in stale_claims.get("items", [])
        if item.get("bead_id")
    }
    active = [
        issue
        for issue in rows
        if issue.get("status") in {"open", "in_progress"}
    ]
    recent_closed = [
        issue
        for issue in rows
        if recent_closed_issue(issue, now=now, max_age_days=recent_closed_after_days)
    ]
    all_surface_paths = tuple(
        sorted({
            surface
            for issue in [*active, *recent_closed]
            for surface in issue_surface_paths(issue)
        })
    )
    if path_activity is None:
        path_activity = collect_git_path_activity(repo_root, all_surface_paths)
    coordination = classify_agent_mail_health(agent_mail_health, agent_mail_health_source)

    groups: list[dict[str, Any]] = []
    seen_group_ids: set[str] = set()
    for left_index, left_issue in enumerate(active):
        left_paths = issue_surface_paths(left_issue)
        for right_issue in active[left_index + 1:]:
            right_paths = issue_surface_paths(right_issue)
            shared = surface_paths_overlap(left_paths, right_paths)
            if not shared:
                continue
            owner_states = {
                owner_state_for_issue(left_issue, stale_claim_items),
                owner_state_for_issue(right_issue, stale_claim_items),
            }
            if owner_states & {
                "ambiguous_missing_coordination",
                "stale_candidate_unassigned",
                "stale_candidate_owner_follow_up",
                "missing_evidence",
            }:
                risk = "stale_owner_overlap"
                confidence = 0.74
                reason = "active/open beads share a surface and at least one owner state is stale or ambiguous"
            elif any(
                dirty_path_matches_surface(dirty, surface)
                for dirty in dirty_paths
                for surface in shared
            ):
                risk = "dirty_surface_overlap"
                confidence = 0.68
                reason = "active/open beads share a surface that also has dirty worktree evidence"
            else:
                risk = "active_surface_collision"
                confidence = 0.86
                reason = "active/open beads share explicit touched-path hints"
            group = redundant_group(
                risk=risk,
                issues=(left_issue, right_issue),
                shared_surfaces=shared,
                match_reason=reason,
                confidence=confidence,
                stale_claim_items=stale_claim_items,
                dirty_worktree_paths=dirty_paths,
                path_activity=path_activity,
                coordination=coordination,
            )
            if group["group_id"] not in seen_group_ids:
                seen_group_ids.add(group["group_id"])
                groups.append(group)

    for active_issue in active:
        active_paths = issue_surface_paths(active_issue)
        for closed_issue in recent_closed:
            closed_paths = issue_surface_paths(closed_issue)
            shared = surface_paths_overlap(active_paths, closed_paths)
            similarity = title_similarity(active_issue, closed_issue)
            if not shared and similarity < 0.5:
                continue
            reason = (
                "open/in-progress bead overlaps a recently closed bead by touched-path hint"
                if shared
                else f"open/in-progress bead title resembles a recently closed bead ({similarity:.2f})"
            )
            group = redundant_group(
                risk="already_closed_duplicate_request",
                issues=(active_issue, closed_issue),
                shared_surfaces=shared,
                match_reason=reason,
                confidence=0.82 if shared else 0.62,
                stale_claim_items=stale_claim_items,
                dirty_worktree_paths=dirty_paths,
                path_activity=path_activity,
                coordination=coordination,
            )
            if group["group_id"] not in seen_group_ids:
                seen_group_ids.add(group["group_id"])
                groups.append(group)

    risk_counts: dict[str, int] = {}
    for group in groups:
        risk = group["risk"]
        risk_counts[risk] = risk_counts.get(risk, 0) + 1

    status = "risks_detected" if groups else "ready"
    if warnings and not groups:
        status = "needs_review"
    return {
        "schema": REDUNDANT_AGENT_WORK_SCHEMA,
        "status": status,
        "policy": "report_only_no_auto_reassign_no_revert_no_delete",
        "source_paths": {
            "beads_ledger": beads_path,
            "agent_mail_health": agent_mail_health_source,
            "dirty_worktree": "git status --porcelain=v1",
            "recent_path_commits": "git log --oneline --max-count=5 -- <path>",
        },
        "coordination": coordination,
        "warnings": warnings,
        "summary": {
            "active_open_count": len(active),
            "recent_closed_reference_count": len(recent_closed),
            "risk_group_count": len(groups),
            "high_confidence_group_count": sum(1 for group in groups if group["confidence"] >= 0.8),
            "risk_counts": risk_counts,
        },
        "groups": groups,
    }


def as_counter(value: Any) -> int:
    if isinstance(value, bool) or value is None:
        return 0
    if isinstance(value, int):
        return max(0, value)
    if isinstance(value, float):
        return max(0, int(value))
    if isinstance(value, str):
        stripped = value.strip()
        if stripped.isdigit():
            return max(0, int(stripped))
    return 0


def read_hostcall_metric(
    reactor: dict[str, Any],
    spec: HostcallQueueMetricSpec,
) -> dict[str, Any]:
    source_path, value = first_path(reactor, spec.paths)
    return {
        "value": value,
        "present": value is not None,
        "source_path": source_path,
        "required": spec.required,
    }


def hostcall_metric_counter(source: dict[str, Any], name: str) -> int:
    metric = source.get("metrics", {}).get(name, {})
    return as_counter(metric.get("value"))


def build_hostcall_source_report(
    repo_root: Path,
    spec: HostcallQueueSourceSpec,
) -> dict[str, Any]:
    full_path = repo_root / spec.path
    base: dict[str, Any] = {
        "id": spec.id,
        "path": spec.path,
        "reactor_path": None,
        "status": "missing_source",
        "metrics": {
            metric.name: {
                "value": None,
                "present": False,
                "source_path": None,
                "required": metric.required,
            }
            for metric in HOSTCALL_QUEUE_METRICS
        },
        "missing_required_fields": [
            metric.name for metric in HOSTCALL_QUEUE_METRICS if metric.required
        ],
        "warnings": [],
        "recommended_operator_action": (
            f"Regenerate {spec.path} before judging hostcall queue fallback behavior."
        ),
    }

    if not full_path.exists():
        return base

    payload, error = load_json(full_path)
    if error is not None:
        base["status"] = "invalid_source"
        base["warnings"] = [error]
        base["recommended_operator_action"] = (
            f"Fix invalid JSON in {spec.path}; hostcall queue telemetry cannot be read."
        )
        return base
    assert payload is not None

    reactor_path, reactor_value = first_path(payload, spec.reactor_paths)
    if not isinstance(reactor_value, dict):
        base["status"] = "missing_telemetry"
        base["recommended_operator_action"] = (
            f"Regenerate {spec.path} with one of {spec.reactor_paths!r}; missing telemetry "
            "is reported explicitly and is not treated as zero fallback pressure."
        )
        return base

    metrics = {
        metric.name: read_hostcall_metric(reactor_value, metric)
        for metric in HOSTCALL_QUEUE_METRICS
    }
    missing_required = [
        metric.name
        for metric in HOSTCALL_QUEUE_METRICS
        if metric.required and not metrics[metric.name]["present"]
    ]
    source_report = {
        **base,
        "reactor_path": reactor_path,
        "metrics": metrics,
        "missing_required_fields": missing_required,
        "warnings": [],
    }

    fallback_total = (
        hostcall_metric_counter(source_report, "s3fifo_fallback_transitions")
        + hostcall_metric_counter(source_report, "s3fifo_fairness_rejected_total")
        + hostcall_metric_counter(source_report, "s3fifo_lane_overflow_rejected_total")
        + hostcall_metric_counter(source_report, "bravo_rollbacks_total")
        + hostcall_metric_counter(source_report, "safe_reclamation_fallback_transitions")
    )
    if missing_required:
        source_report["status"] = "missing_fields"
        source_report["recommended_operator_action"] = (
            f"Regenerate {spec.path} with stable hostcall queue fields: "
            f"{', '.join(missing_required)}."
        )
    elif fallback_total:
        source_report["status"] = "fallback_heavy"
        source_report["recommended_operator_action"] = (
            "Inspect S3-FIFO fairness, lane overflow, safe-fallback, and BRAVO rollback "
            "counters before presenting this run as swarm-ready."
        )
    else:
        source_report["status"] = "ready"
        source_report["recommended_operator_action"] = (
            "No hostcall queue fallback pressure is visible in this source."
        )
    return source_report


def build_hostcall_queue_report(repo_root: Path) -> dict[str, Any]:
    sources = [
        build_hostcall_source_report(repo_root, source_spec)
        for source_spec in HOSTCALL_QUEUE_SOURCES
    ]
    status_counts: dict[str, int] = {}
    for source in sources:
        status = source["status"]
        status_counts[status] = status_counts.get(status, 0) + 1

    missing_required_count = sum(
        len(source["missing_required_fields"]) for source in sources
    )
    summary = {
        "sources_checked": len(sources),
        "sources_with_hostcall_queue_telemetry": sum(
            1 for source in sources if source["reactor_path"] is not None
        ),
        "missing_required_field_count": missing_required_count,
        "s3fifo_fallback_transitions": sum(
            hostcall_metric_counter(source, "s3fifo_fallback_transitions")
            for source in sources
        ),
        "s3fifo_fairness_rejected_total": sum(
            hostcall_metric_counter(source, "s3fifo_fairness_rejected_total")
            for source in sources
        ),
        "s3fifo_lane_overflow_rejected_total": sum(
            hostcall_metric_counter(source, "s3fifo_lane_overflow_rejected_total")
            for source in sources
        ),
        "queue_overflow_rejected_total": sum(
            hostcall_metric_counter(source, "queue_overflow_rejected_total")
            for source in sources
        ),
        "safe_reclamation_fallback_transitions": sum(
            hostcall_metric_counter(source, "safe_reclamation_fallback_transitions")
            for source in sources
        ),
        "bravo_transitions_total": sum(
            hostcall_metric_counter(source, "bravo_transitions_total")
            for source in sources
        ),
        "bravo_rollbacks_total": sum(
            hostcall_metric_counter(source, "bravo_rollbacks_total")
            for source in sources
        ),
        "status_counts": status_counts,
    }
    fallback_heavy = any(
        summary[key] > 0
        for key in (
            "s3fifo_fallback_transitions",
            "s3fifo_fairness_rejected_total",
            "s3fifo_lane_overflow_rejected_total",
            "safe_reclamation_fallback_transitions",
            "bravo_rollbacks_total",
        )
    )

    if missing_required_count or status_counts.get("missing_source") or status_counts.get("invalid_source"):
        status = "needs_telemetry"
    elif fallback_heavy:
        status = "fallback_heavy"
    else:
        status = "ready"

    return {
        "schema": HOSTCALL_QUEUE_REPORT_SCHEMA,
        "status": status,
        "policy": "read_only_no_gate_side_effect",
        "source_paths": [source.path for source in HOSTCALL_QUEUE_SOURCES],
        "required_fields": [
            metric.name for metric in HOSTCALL_QUEUE_METRICS if metric.required
        ],
        "summary": summary,
        "sources": sources,
    }


def check_spec(
    repo_root: Path,
    spec: EvidenceSpec,
    now: datetime,
    max_age: timedelta,
    *,
    expected_release_head: str | None = None,
) -> EvidenceCheck:
    full_path = repo_root / spec.path
    issues: list[EvidenceIssue] = []
    generated_at: datetime | None = None
    age_days: float | None = None
    schema: str | None = None
    provenance_value: str | None = None

    if not full_path.exists():
        return EvidenceCheck(
            spec=spec,
            exists=False,
            generated_at=None,
            age_days=None,
            schema=None,
            provenance_value=None,
            issues=[issue_for(spec, "missing", "artifact path does not exist")],
            parsed_payload=None,
        )

    release_snapshot: ReleaseArtifactSnapshot | None = None
    if spec.claim_surface == "release_facing":
        release_snapshot, binding_error = capture_release_artifact_snapshot(
            repo_root, spec.path
        )
        if binding_error is not None:
            issues.append(issue_for(spec, "source_binding", binding_error))
        elif (
            release_snapshot is not None
            and expected_release_head is not None
            and release_snapshot.head != expected_release_head
        ):
            issues.append(issue_for(
                spec,
                "source_binding",
                (
                    "artifact was captured from a different repository HEAD than "
                    f"the report snapshot ({release_snapshot.head} != "
                    f"{expected_release_head})"
                ),
            ))

    payload: dict[str, Any] | None = None
    if spec.path.endswith(".json"):
        if release_snapshot is not None:
            payload, json_error = load_json_bytes(release_snapshot.contents)
        else:
            payload, json_error = load_json(full_path)
        if json_error is not None:
            issues.append(issue_for(spec, "invalid_json", json_error))
        if payload is not None:
            raw_schema = payload.get("schema")
            if isinstance(raw_schema, str):
                schema = raw_schema
            if spec.required_schema is not None and schema != spec.required_schema:
                issues.append(issue_for(
                    spec,
                    "schema_mismatch",
                    f"expected schema {spec.required_schema!r}, found {schema!r}",
                ))

            _, raw_generated_at = first_path(payload, spec.timestamp_paths)
            generated_at = parse_iso_datetime(raw_generated_at)
            _, raw_provenance = first_path(payload, spec.provenance_paths)
            if raw_provenance is not None:
                provenance_value = str(raw_provenance)

            if spec.status_path is not None:
                raw_status = normalize_value(get_path(payload, spec.status_path))
                ok_values = tuple(normalize_value(value) for value in spec.ok_values)
                if raw_status not in ok_values:
                    issues.append(issue_for(
                        spec,
                        "status_not_ready",
                        f"{spec.status_path}={raw_status!r}, expected one of {ok_values!r}",
                    ))

            for zero_path in spec.zero_paths:
                detail = zero_path_issue_detail(spec, payload, zero_path)
                if detail is not None:
                    issues.append(issue_for(spec, "no_data", detail))

            for required_true_path in spec.required_true_paths:
                value = get_path(payload, required_true_path)
                if value is not True:
                    issues.append(issue_for(
                        spec,
                        "status_not_ready",
                        f"{required_true_path}={value!r}, expected True",
                    ))

            for required_empty_list_path in spec.required_empty_list_paths:
                value = get_path(payload, required_empty_list_path)
                if not isinstance(value, list) or value:
                    issues.append(issue_for(
                        spec,
                        "status_not_ready",
                        f"{required_empty_list_path}={value!r}, expected an empty array",
                    ))

            if spec.id == "perf_budget_summary":
                issues.extend(
                    performance_budget_contract_issues(repo_root, spec, payload, now)
                )

    if release_snapshot is not None:
        final_binding_error = revalidate_release_artifact_snapshot(release_snapshot)
        if final_binding_error is not None:
            issues.append(issue_for(spec, "source_binding", final_binding_error))

    if generated_at is None and spec.generated:
        issues.append(issue_for(spec, "missing_timestamp", "artifact lacks a parseable generated timestamp"))

    if generated_at is not None:
        age = now - generated_at
        age_days = age.total_seconds() / 86400
        if generated_at > now + timedelta(minutes=5):
            issues.append(issue_for(
                spec,
                "future_timestamp",
                "generated timestamp is more than five minutes in the future",
            ))
        elif age > max_age and spec.claim_surface != "release_policy":
            issues.append(issue_for(
                spec,
                "stale",
                f"generated timestamp is {age_days:.1f} days old; limit is {max_age.days} days",
            ))

    return EvidenceCheck(
        spec=spec,
        exists=True,
        generated_at=generated_at,
        age_days=age_days,
        schema=schema,
        provenance_value=provenance_value,
        issues=issues,
        release_snapshot=release_snapshot,
        parsed_payload=payload,
    )


def add_provenance_mismatches(checks: list[EvidenceCheck]) -> None:
    groups: dict[str, dict[str, list[EvidenceCheck]]] = {}
    for check in checks:
        group = check.spec.provenance_group
        if group is None or not check.provenance_value or check.blocking_issue_count() > 0:
            continue
        groups.setdefault(group, {}).setdefault(check.provenance_value, []).append(check)

    for group, values in groups.items():
        if len(values) <= 1:
            continue
        summary = ", ".join(
            f"{value}: {[check.spec.path for check in group_checks]}"
            for value, group_checks in sorted(values.items())
        )
        for group_checks in values.values():
            for check in group_checks:
                check.issues.append(issue_for(
                    check.spec,
                    "provenance_mismatch",
                    f"{group} evidence has multiple provenance values: {summary}",
                ))


def category_summary(checks: list[EvidenceCheck]) -> list[dict[str, Any]]:
    categories = sorted({check.spec.category for check in checks})
    summaries: list[dict[str, Any]] = []
    for category in categories:
        category_checks = [check for check in checks if check.spec.category == category]
        blocking = sum(check.blocking_issue_count() for check in category_checks)
        statuses: dict[str, int] = {}
        for check in category_checks:
            statuses[check.status()] = statuses.get(check.status(), 0) + 1
        summaries.append({
            "category": category,
            "status": "blocked" if blocking else "ready",
            "blocking_issues": blocking,
            "statuses": statuses,
        })
    return summaries


def remediation_for(kind: str) -> str:
    return {
        "missing": "Regenerate the artifact at the exact path or remove/soften the claim that cites it.",
        "invalid_json": "Fix the artifact writer or replace the malformed artifact with a valid generated artifact.",
        "schema_mismatch": "Regenerate with the current schema before using the artifact for release claims.",
        "status_not_ready": "Do not make the release-facing claim until the gate verdict is passing.",
        "no_data": "Regenerate the evidence from a run with real measurements and clean data contracts.",
        "missing_timestamp": "Regenerate with generated_at or generated_at_utc provenance.",
        "future_timestamp": (
            "Regenerate the evidence with the current UTC clock and truthful provenance."
        ),
        "stale": "Regenerate the evidence and update claim citations, or soften the claim to historical language.",
        "provenance_mismatch": "Use a single correlated evidence run for the claim or split the claim by run.",
        "source_binding": (
            "Regenerate and commit the exact release-facing artifact as a regular HEAD blob "
            "with no symlink components or worktree byte drift."
        ),
        "readme_snapshot_mismatch": "Update README.md release-facing snapshot text to match the current evidence artifacts.",
        "readme_snapshot_missing": "Restore the README.md release-facing snapshot section or remove release-facing snapshot claims.",
    }.get(kind, "Review the artifact before using it for release claims.")


def blocker_category(kind: str, detail: str) -> str:
    lowered = f"{kind} {detail}".lower()
    if "rch" in lowered or "remote compilation" in lowered or "workspace shadow" in lowered:
        return "rch_failure"
    if kind in {"missing", "readme_snapshot_missing"}:
        return "missing_artifact"
    if kind in {"stale", "future_timestamp"}:
        return "stale_evidence"
    if kind == "no_data":
        return "no_data_evidence"
    if kind == "provenance_mismatch":
        return "provenance_mismatch"
    if kind in {"invalid_json", "schema_mismatch", "source_binding"}:
        return "invalid_evidence_contract"
    if kind == "status_not_ready":
        return "release_gate_not_ready"
    if kind == "missing_timestamp":
        return "missing_evidence_timestamp"
    if kind == "readme_snapshot_mismatch":
        return "claim_snapshot_mismatch"
    return "validation_blocker"


def blocker_explanation(category: str) -> str:
    return {
        "stale_evidence": (
            "The report found release-facing evidence whose generated timestamp is beyond "
            "the freshness window, so current claims cannot rely on it."
        ),
        "missing_artifact": (
            "A required release-facing artifact or README evidence section is absent at "
            "the exact path the claim expects."
        ),
        "rch_failure": (
            "A validation command or evidence row points at an RCH or remote-workspace "
            "failure, so the proof is not authoritative for the current worktree."
        ),
        "dirty_worktree_conflict": (
            "Dirty files touch an active bead surface; this is fresh work evidence and a "
            "conflict risk for other agents."
        ),
        "agent_mail_semantic_corruption": (
            "Coordination evidence is missing or degraded. Treat Agent Mail corruption as "
            "unknown owner state, not proof that the assignee is gone."
        ),
        "redundant_agent_work": (
            "Two or more Beads, dirty paths, or recent closed work appear to cover the same "
            "surface. The report is advisory and requires manual coordination before work starts."
        ),
        "no_data_evidence": (
            "The evidence artifact exists but reports zero, skipped, or missing measurement "
            "data, so it cannot support a green claim."
        ),
        "provenance_mismatch": (
            "Evidence for one claim surface comes from multiple unrelated runs, which makes "
            "the combined claim non-replayable."
        ),
        "invalid_evidence_contract": (
            "The evidence artifact does not match the expected machine contract and must not "
            "be used as proof."
        ),
        "release_gate_not_ready": (
            "A release-facing gate explicitly reports a non-passing verdict."
        ),
        "missing_evidence_timestamp": (
            "The evidence lacks generated_at provenance, so freshness cannot be verified."
        ),
        "claim_snapshot_mismatch": (
            "README claim text no longer matches the checked-in evidence snapshot."
        ),
        "validation_blocker": (
            "A release-facing validation blocker requires operator action before the claim "
            "can be treated as green."
        ),
    }[category]


def explanation_item(
    *,
    source: str,
    category: str,
    affected: str,
    detail: str,
    next_safe_action: str,
) -> dict[str, Any]:
    return {
        "id": f"{category}:{source}:{affected}",
        "category": category,
        "source": source,
        "affected": affected,
        "detail": detail,
        "explanation": blocker_explanation(category),
        "next_safe_action": next_safe_action,
        "claim_integrity": "not_green_until_resolved",
    }


def build_operator_explanations(
    *,
    blocking_issues: list[dict[str, Any]],
    stale_claims: dict[str, Any],
    redundant_agent_work: dict[str, Any] | None = None,
) -> dict[str, Any]:
    items: list[dict[str, Any]] = []
    for issue in blocking_issues:
        kind = str(issue.get("kind") or "validation_blocker")
        detail = str(issue.get("detail") or "")
        category = blocker_category(kind, detail)
        affected = str(issue.get("path") or "unknown")
        if issue.get("line") is not None:
            affected = f"{affected}:{issue['line']}"
        items.append(explanation_item(
            source=f"blocking_issue:{kind}",
            category=category,
            affected=affected,
            detail=detail,
            next_safe_action=str(issue.get("remediation") or remediation_for(kind)),
        ))

    for item in stale_claims.get("items", []):
        bead_id = str(item.get("bead_id") or "unknown")
        classification = str(item.get("classification") or "")
        if classification == "active_dirty_worktree":
            detail = "; ".join(str(path) for path in item.get("dirty_worktree_paths", []))
            items.append(explanation_item(
                source=f"stale_claim:{classification}",
                category="dirty_worktree_conflict",
                affected=f"bead:{bead_id}",
                detail=detail or "dirty worktree touches the active bead surface",
                next_safe_action=str(item.get("recommended_operator_action") or ""),
            ))
        elif classification in {"ambiguous_missing_coordination", "missing_evidence"}:
            items.append(explanation_item(
                source=f"stale_claim:{classification}",
                category="agent_mail_semantic_corruption",
                affected=f"bead:{bead_id}",
                detail="; ".join(str(reason) for reason in item.get("reasons", [])),
                next_safe_action=str(item.get("recommended_operator_action") or ""),
            ))
        elif str(item.get("operator_bucket") or "") == "stale_candidate":
            items.append(explanation_item(
                source=f"stale_claim:{classification}",
                category="validation_blocker",
                affected=f"bead:{bead_id}",
                detail="; ".join(str(reason) for reason in item.get("reasons", [])),
                next_safe_action=str(item.get("recommended_operator_action") or ""),
            ))

    if redundant_agent_work is not None:
        coordination = redundant_agent_work.get("coordination")
        if (
            isinstance(coordination, dict)
            and coordination.get("classification") == "degraded_agent_mail"
        ):
            source = str(coordination.get("source") or "agent_mail_health")
            items.append(explanation_item(
                source=f"agent_mail_health:{source}",
                category="agent_mail_semantic_corruption",
                affected="agent_mail",
                detail=str(coordination.get("detail") or ""),
                next_safe_action=str(
                    coordination.get("recovery_action")
                    or "Use Beads comments/status as the soft lock until Agent Mail is repaired."
                ),
            ))
        for group in redundant_agent_work.get("groups", []):
            members = [
                str(member.get("bead_id") or "unknown")
                for member in group.get("members", [])
            ]
            affected = ",".join(members) if members else str(group.get("group_id") or "unknown")
            items.append(explanation_item(
                source=f"redundant_agent_work:{group.get('risk') or 'overlap'}",
                category="redundant_agent_work",
                affected=f"beads:{affected}",
                detail=str(group.get("match_reason") or ""),
                next_safe_action=str(group.get("recommended_operator_action") or ""),
            ))

    category_counts: dict[str, int] = {}
    for item in items:
        category = item["category"]
        category_counts[category] = category_counts.get(category, 0) + 1

    return {
        "schema": OPERATOR_EXPLANATIONS_SCHEMA,
        "status": "needs_action" if items else "ready",
        "policy": "degraded_or_stale_evidence_is_never_green",
        "summary": {
            "explanation_count": len(items),
            "category_counts": category_counts,
        },
        "items": items,
    }


def readme_timestamp_text(payload: dict[str, Any], paths: tuple[str, ...] = DEFAULT_TIMESTAMP_PATHS) -> str | None:
    for path in paths:
        raw = get_path(payload, path)
        if isinstance(raw, str) and raw.strip():
            return raw.strip()
        parsed = parse_iso_datetime(raw)
        if parsed is not None:
            return format_datetime(parsed)
    return None


def readme_date_text(value: str | None) -> str | None:
    if value is None or len(value) < 10:
        return None
    return value[:10]


def readme_count(payload: dict[str, Any], path: str) -> int | None:
    return non_negative_count(get_path(payload, path))


def readme_json_artifact(
    evidence_checks_by_path: dict[str, EvidenceCheck],
    relative_path: str,
) -> dict[str, Any]:
    evidence_check = evidence_checks_by_path.get(relative_path)
    if evidence_check is None or evidence_check.parsed_payload is None:
        return {}
    return evidence_check.parsed_payload


def readme_gate_counts(payload: dict[str, Any]) -> tuple[int | None, int | None, int | None, int | None]:
    hard_gates = get_path(payload, "hard_gate_results")
    if isinstance(hard_gates, list):
        total = 0
        passed = 0
        blocking_total = 0
        blocking_passed = 0
        for gate in hard_gates:
            if not isinstance(gate, dict):
                continue
            total += 1
            is_pass = str(gate.get("status", "")).lower() == "pass"
            if is_pass:
                passed += 1
            if gate.get("blocking") is True:
                blocking_total += 1
                if is_pass:
                    blocking_passed += 1
        return total, passed, blocking_total, blocking_passed

    summary = get_path(payload, "summary")
    if isinstance(summary, dict):
        return (
            readme_count(summary, "total_gates"),
            readme_count(summary, "passed"),
            readme_count(summary, "blocking_total"),
            readme_count(summary, "blocking_pass"),
        )
    return None, None, None, None


def add_readme_check(
    checks: list[ReadmeLineCheck],
    *,
    id: str,
    section_id: str,
    anchor: str,
    expected: str | None,
    source_path: str,
    description: str,
) -> None:
    if expected:
        checks.append(ReadmeLineCheck(
            id=id,
            section_id=section_id,
            anchor=anchor,
            expected=expected,
            source_path=source_path,
            description=description,
        ))


def build_readme_line_checks(
    evidence_checks_by_path: dict[str, EvidenceCheck],
) -> list[ReadmeLineCheck]:
    must_pass_path = "tests/ext_conformance/reports/gate/must_pass_gate_verdict.json"
    evidence_bundle_path = "tests/evidence_bundle/index.json"
    full_suite_path = "tests/full_suite_gate/full_suite_verdict.json"
    certification_path = "tests/full_suite_gate/certification_verdict.json"
    dropin_path = "docs/evidence/dropin-certification-verdict.json"

    must_pass = readme_json_artifact(evidence_checks_by_path, must_pass_path)
    evidence_bundle = readme_json_artifact(
        evidence_checks_by_path,
        evidence_bundle_path,
    )
    full_suite = readme_json_artifact(evidence_checks_by_path, full_suite_path)
    certification = readme_json_artifact(
        evidence_checks_by_path,
        certification_path,
    )
    dropin = readme_json_artifact(evidence_checks_by_path, dropin_path)

    checks: list[ReadmeLineCheck] = []

    must_pass_generated = readme_timestamp_text(must_pass)
    must_pass_date = readme_date_text(must_pass_generated)
    must_pass_run_id = get_path(must_pass, "run_id")
    must_pass_passed = readme_count(must_pass, "observed.must_pass_passed")
    must_pass_total = readme_count(must_pass, "observed.must_pass_total")
    stretch_passed = readme_count(must_pass, "observed.stretch_passed")
    stretch_total = readme_count(must_pass, "observed.stretch_total")

    evidence_generated = readme_timestamp_text(evidence_bundle)
    evidence_run_id = get_path(evidence_bundle, "ci_run_id")
    evidence_total = readme_count(evidence_bundle, "summary.total_sections")
    evidence_present = readme_count(evidence_bundle, "summary.present_sections")
    evidence_missing = readme_count(evidence_bundle, "summary.missing_sections")
    evidence_invalid = readme_count(evidence_bundle, "summary.invalid_sections")

    certification_generated = readme_timestamp_text(certification)
    dropin_generated = readme_timestamp_text(dropin)
    dropin_date = readme_date_text(dropin_generated)
    dropin_total, dropin_passed, dropin_blocking_total, dropin_blocking_passed = readme_gate_counts(dropin)
    full_suite_total, full_suite_passed, full_suite_blocking_total, full_suite_blocking_passed = readme_gate_counts(full_suite)
    dropin_verdict = get_path(dropin, "overall_verdict")

    text_values = {
        "must_pass_date": must_pass_date,
        "dropin_date": dropin_date,
        "must_pass_generated": f"generated `{must_pass_generated}`" if must_pass_generated else None,
        "must_pass_run_id": f"run `{must_pass_run_id}`"
        if isinstance(must_pass_run_id, str) and must_pass_run_id
        else None,
        "evidence_generated": f"generated `{evidence_generated}`" if evidence_generated else None,
        "evidence_run_id": f"run `{evidence_run_id}`"
        if isinstance(evidence_run_id, str) and evidence_run_id
        else None,
        "certification_generated": (
            f"generated `{certification_generated}`" if certification_generated else None
        ),
        "dropin_generated": f"generated `{dropin_generated}`" if dropin_generated else None,
        "dropin_status_counts": (
            f"**{dropin_passed}/{dropin_total} certification gates PASS, "
            f"{dropin_blocking_passed}/{dropin_blocking_total} blocking gates PASS**"
            if None not in (dropin_total, dropin_passed, dropin_blocking_total, dropin_blocking_passed)
            else None
        ),
        "dropin_certification_counts": (
            f"`{dropin_passed}/{dropin_total}` certification gates passed"
            if None not in (dropin_total, dropin_passed)
            else None
        ),
        "dropin_verdict": f"`{dropin_verdict}`"
        if isinstance(dropin_verdict, str) and dropin_verdict
        else None,
        "evidence_sections": (
            f"`{evidence_present}/{evidence_total}` sections present"
            if None not in (evidence_present, evidence_total)
            else None
        ),
        "evidence_missing": f"`{evidence_missing}` missing"
        if evidence_missing is not None
        else None,
        "evidence_invalid": f"`{evidence_invalid}` invalid"
        if evidence_invalid is not None
        else None,
        "must_pass_counts": (
            f"`{must_pass_passed}/{must_pass_total}` must-pass extensions passed"
            if None not in (must_pass_passed, must_pass_total)
            else None
        ),
        "stretch_counts": (
            f"`{stretch_passed}/{stretch_total}` passed"
            if None not in (stretch_passed, stretch_total)
            else None
        ),
        "full_suite_counts": (
            f"`{full_suite_passed}/{full_suite_total}` gates passed, including "
            f"`{full_suite_blocking_passed}/{full_suite_blocking_total}` blocking gates"
            if None not in (
                full_suite_total,
                full_suite_passed,
                full_suite_blocking_total,
                full_suite_blocking_passed,
            )
            else None
        ),
    }

    check_rows = (
        ("latest_header_extension_date", "latest_run_snapshot", "### Latest run snapshot", "must_pass_date", must_pass_path),
        ("latest_extension_generated_at", "latest_run_snapshot", must_pass_path, "must_pass_generated", must_pass_path),
        ("latest_extension_run_id", "latest_run_snapshot", must_pass_path, "must_pass_run_id", must_pass_path),
        ("latest_evidence_generated_at", "latest_run_snapshot", evidence_bundle_path, "evidence_generated", evidence_bundle_path),
        ("latest_evidence_run_id", "latest_run_snapshot", evidence_bundle_path, "evidence_run_id", evidence_bundle_path),
        ("latest_certification_generated_at", "latest_run_snapshot", certification_path, "certification_generated", certification_path),
        ("latest_dropin_generated_at", "latest_run_snapshot", dropin_path, "dropin_generated", dropin_path),
        ("latest_dropin_gate_counts", "latest_run_snapshot", "Strict drop-in status", "dropin_status_counts", dropin_path),
        ("latest_dropin_verdict", "latest_run_snapshot", "Strict drop-in status", "dropin_verdict", dropin_path),
        ("latest_evidence_bundle_sections", "latest_run_snapshot", "Unified evidence bundle", "evidence_sections", evidence_bundle_path),
        ("latest_evidence_bundle_missing", "latest_run_snapshot", "Unified evidence bundle", "evidence_missing", evidence_bundle_path),
        ("latest_evidence_bundle_invalid", "latest_run_snapshot", "Unified evidence bundle", "evidence_invalid", evidence_bundle_path),
        ("latest_must_pass_counts", "latest_run_snapshot", "Extension must-pass gate", "must_pass_counts", must_pass_path),
        ("latest_stretch_counts", "latest_run_snapshot", "Extension must-pass gate", "stretch_counts", must_pass_path),
        ("refresh_header_extension_date", "certification_evidence_refresh", "Latest certification/evidence refresh", "must_pass_date", must_pass_path),
        ("refresh_header_dropin_date", "certification_evidence_refresh", "Latest certification/evidence refresh", "dropin_date", dropin_path),
        ("refresh_evidence_bundle_sections", "certification_evidence_refresh", "Unified evidence bundle", "evidence_sections", evidence_bundle_path),
        ("refresh_evidence_bundle_missing", "certification_evidence_refresh", "Unified evidence bundle", "evidence_missing", evidence_bundle_path),
        ("refresh_evidence_bundle_invalid", "certification_evidence_refresh", "Unified evidence bundle", "evidence_invalid", evidence_bundle_path),
        ("refresh_full_suite_counts", "certification_evidence_refresh", "Full-suite gate", "full_suite_counts", full_suite_path),
        ("refresh_dropin_gate_counts", "certification_evidence_refresh", "Drop-in certification", "dropin_certification_counts", dropin_path),
        ("refresh_dropin_verdict", "certification_evidence_refresh", "Drop-in certification", "dropin_verdict", dropin_path),
        ("refresh_must_pass_counts", "certification_evidence_refresh", "Extension must-pass gate", "must_pass_counts", must_pass_path),
        ("refresh_stretch_counts", "certification_evidence_refresh", "Extension must-pass gate", "stretch_counts", must_pass_path),
    )
    for check_id, section_id, anchor, value_key, source_path in check_rows:
        add_readme_check(
            checks,
            id=check_id,
            section_id=section_id,
            anchor=anchor,
            expected=text_values[value_key],
            source_path=source_path,
            description=check_id.replace("_", " "),
        )
    return checks


def readme_section_bounds(
    lines: list[str],
    spec: ReadmeSectionSpec,
) -> tuple[int, int, int | None]:
    start_index = None
    for index, line in enumerate(lines):
        if spec.start_marker in line:
            start_index = index
            break
    if start_index is None:
        return 0, 0, None

    end_index = len(lines)
    for index in range(start_index + 1, len(lines)):
        if spec.end_marker in lines[index]:
            end_index = index
            break
    return start_index, end_index, start_index + 1


def find_readme_anchor(
    lines: list[str],
    start_index: int,
    end_index: int,
    anchor: str,
) -> tuple[int | None, str | None]:
    for index in range(start_index, end_index):
        if anchor in lines[index]:
            return index + 1, lines[index].strip()
    return None, None


def check_readme_snapshot_artifact_citations(
    evidence_checks_by_path: dict[str, EvidenceCheck],
    lines: list[str],
    section_specs: dict[str, ReadmeSectionSpec],
    bounds: dict[str, tuple[int, int, int | None]],
) -> list[dict[str, Any]]:
    issues: list[dict[str, Any]] = []
    seen: set[tuple[str, int, str]] = set()
    for section_id, (start_index, end_index, section_line) in bounds.items():
        if section_line is None:
            continue
        for line_index in range(start_index, end_index):
            line = lines[line_index]
            line_number = line_index + 1
            for match in README_SNAPSHOT_JSON_PATH_RE.finditer(line):
                artifact_path = match.group(1)
                key = (section_id, line_number, artifact_path)
                if key in seen:
                    continue
                seen.add(key)

                evidence_check = evidence_checks_by_path.get(artifact_path)
                if evidence_check is None or not evidence_check.exists:
                    issues.append({
                        "path": "README.md",
                        "line": line_number,
                        "category": "docs",
                        "kind": "readme_snapshot_missing",
                        "detail": (
                            f"{section_id} cites missing artifact {artifact_path!r} "
                            f"in section {section_specs[section_id].start_marker!r}"
                        ),
                        "remediation": remediation_for("readme_snapshot_missing"),
                    })
                    continue

                generated_match = README_SNAPSHOT_GENERATED_RE.match(line[match.end():])
                if generated_match is None:
                    continue

                payload = readme_json_artifact(
                    evidence_checks_by_path,
                    artifact_path,
                )
                actual_generated = readme_timestamp_text(payload)
                expected_generated = generated_match.group(1)
                if actual_generated != expected_generated:
                    issues.append({
                        "path": "README.md",
                        "line": line_number,
                        "category": "docs",
                        "kind": "readme_snapshot_mismatch",
                        "detail": (
                            f"{section_id} citation for {artifact_path} expected generated "
                            f"{actual_generated!r}; actual line cites {expected_generated!r}"
                        ),
                        "remediation": remediation_for("readme_snapshot_mismatch"),
                    })

                expected_run = generated_match.group(2)
                if expected_run is None:
                    continue
                actual_run = get_path(payload, "run_id") or get_path(payload, "ci_run_id")
                if isinstance(actual_run, str) and actual_run != expected_run:
                    issues.append({
                        "path": "README.md",
                        "line": line_number,
                        "category": "docs",
                        "kind": "readme_snapshot_mismatch",
                        "detail": (
                            f"{section_id} citation for {artifact_path} expected run "
                            f"{actual_run!r}; actual line cites {expected_run!r}"
                        ),
                        "remediation": remediation_for("readme_snapshot_mismatch"),
                    })
    return issues


def check_readme_release_snapshot(
    evidence_checks_by_path: dict[str, EvidenceCheck],
    *,
    readme_snapshot: ReleaseArtifactSnapshot | None,
    binding_error: str | None,
    _before_expectations: Callable[[], None] | None = None,
    _after_expectations: Callable[[], None] | None = None,
) -> list[dict[str, Any]]:
    if binding_error is not None or readme_snapshot is None:
        return [{
            "path": "README.md",
            "line": None,
            "category": "docs",
            "kind": "source_binding",
            "detail": binding_error or "README.md snapshot capture failed",
            "remediation": remediation_for("source_binding"),
        }]
    try:
        lines = readme_snapshot.contents.decode("utf-8", "strict").splitlines()
    except UnicodeDecodeError as exc:
        return [{
            "path": "README.md",
            "line": None,
            "category": "docs",
            "kind": "readme_snapshot_missing",
            "detail": f"README.md is not UTF-8: {exc}",
            "remediation": remediation_for("readme_snapshot_missing"),
        }]

    issues: list[dict[str, Any]] = []
    try:
        if _before_expectations is not None:
            _before_expectations()
        section_specs = {spec.id: spec for spec in README_SECTIONS}
        bounds = {
            section_id: readme_section_bounds(lines, spec)
            for section_id, spec in section_specs.items()
        }
        for section_id, (_, _, section_line) in bounds.items():
            if section_line is None:
                issues.append({
                    "path": "README.md",
                    "line": None,
                    "category": "docs",
                    "kind": "readme_snapshot_missing",
                    "detail": (
                        f"README.md is missing release snapshot section "
                        f"{section_specs[section_id].start_marker!r}"
                    ),
                    "remediation": remediation_for("readme_snapshot_missing"),
                })

        for check in build_readme_line_checks(evidence_checks_by_path):
            if check.section_id not in bounds:
                continue
            start_index, end_index, section_line = bounds[check.section_id]
            if section_line is None:
                continue
            line_number, actual = find_readme_anchor(
                lines,
                start_index,
                end_index,
                check.anchor,
            )
            if actual is None:
                issues.append({
                    "path": "README.md",
                    "line": section_line,
                    "category": "docs",
                    "kind": "readme_snapshot_mismatch",
                    "detail": (
                        f"{check.section_id}.{check.id} ({check.description}) could not find anchor "
                        f"{check.anchor!r}; expected {check.expected!r} from {check.source_path}"
                    ),
                    "remediation": remediation_for("readme_snapshot_mismatch"),
                })
                continue
            if check.expected not in actual:
                issues.append({
                    "path": "README.md",
                    "line": line_number,
                    "category": "docs",
                    "kind": "readme_snapshot_mismatch",
                    "detail": (
                        f"{check.section_id}.{check.id} ({check.description}) expected {check.expected!r} "
                        f"from {check.source_path}; actual line is {actual!r}"
                    ),
                    "remediation": remediation_for("readme_snapshot_mismatch"),
                })
        issues.extend(check_readme_snapshot_artifact_citations(
            evidence_checks_by_path,
            lines,
            section_specs,
            bounds,
        ))
    finally:
        if _after_expectations is not None:
            _after_expectations()
    final_binding_error = revalidate_release_artifact_snapshot(readme_snapshot)
    if final_binding_error is not None:
        issues.append({
            "path": "README.md",
            "line": None,
            "category": "docs",
            "kind": "source_binding",
            "detail": final_binding_error,
            "remediation": remediation_for("source_binding"),
        })
    return issues


def build_report(
    repo_root: Path,
    *,
    now: datetime | None = None,
    max_age_days: int = DEFAULT_MAX_AGE_DAYS,
    stale_claim_after_hours: int = DEFAULT_STALE_CLAIM_AFTER_HOURS,
    stale_claim_activity_fresh_hours: int = DEFAULT_STALE_CLAIM_ACTIVITY_FRESH_HOURS,
    stale_claim_activity_paths: tuple[str, ...] = DEFAULT_STALE_CLAIM_ACTIVITY_PATHS,
    stale_claim_dirty_worktree_paths: tuple[str, ...] | None = None,
    stale_claim_recent_commit_activity: dict[str, ClaimActivityEvidence] | None = None,
    redundant_agent_mail_health: dict[str, Any] | None = None,
    redundant_agent_mail_health_source: str = "not_provided",
    redundant_path_activity: dict[str, list[str]] | None = None,
    _after_evidence_check: Callable[[int], None] | None = None,
    _before_readme_expectations: Callable[[], None] | None = None,
    _after_readme_expectations: Callable[[], None] | None = None,
    _before_final_binding_check: Callable[[], None] | None = None,
) -> dict[str, Any]:
    now = as_utc(now or datetime.now(timezone.utc))
    max_age = timedelta(days=max_age_days)
    report_snapshot, report_snapshot_error = capture_release_artifact_snapshot(
        repo_root,
        "README.md",
    )
    expected_release_head = (
        report_snapshot.head if report_snapshot is not None else None
    )
    checks: list[EvidenceCheck] = []
    for index, spec in enumerate(EVIDENCE_SPECS):
        checks.append(check_spec(
            repo_root,
            spec,
            now,
            max_age,
            expected_release_head=expected_release_head,
        ))
        if _after_evidence_check is not None:
            _after_evidence_check(index)
    add_provenance_mismatches(checks)
    stale_claims = build_stale_claim_report(
        repo_root,
        now=now,
        stale_after_hours=stale_claim_after_hours,
        activity_fresh_after_hours=stale_claim_activity_fresh_hours,
        activity_paths=stale_claim_activity_paths,
        dirty_worktree_paths=stale_claim_dirty_worktree_paths,
        recent_commit_activity=stale_claim_recent_commit_activity,
    )
    hostcall_queue_telemetry = build_hostcall_queue_report(repo_root)
    redundant_agent_work = build_redundant_agent_work_report(
        repo_root,
        now=now,
        stale_claims=stale_claims,
        agent_mail_health=redundant_agent_mail_health,
        agent_mail_health_source=redundant_agent_mail_health_source,
        dirty_worktree_paths=stale_claim_dirty_worktree_paths,
        path_activity=redundant_path_activity,
    )

    readme_blocking_issues = check_readme_release_snapshot(
        {check.spec.path: check for check in checks},
        readme_snapshot=report_snapshot,
        binding_error=report_snapshot_error,
        _before_expectations=_before_readme_expectations,
        _after_expectations=_after_readme_expectations,
    )
    if _before_final_binding_check is not None:
        _before_final_binding_check()
    for check in checks:
        if check.release_snapshot is None:
            continue
        final_binding_error = revalidate_release_artifact_snapshot(
            check.release_snapshot
        )
        if final_binding_error is not None:
            check.issues.append(issue_for(
                check.spec,
                "source_binding",
                (
                    "release-facing artifact changed after its evidence check "
                    f"while the report was being assembled: {final_binding_error}"
                ),
            ))
    if report_snapshot is not None:
        final_report_binding_error = revalidate_release_artifact_snapshot(
            report_snapshot
        )
        if final_report_binding_error is not None:
            readme_blocking_issues.append({
                "path": "README.md",
                "line": None,
                "category": "docs",
                "kind": "source_binding",
                "detail": (
                    "repository changed while the claim-readiness report was "
                    f"being assembled: {final_report_binding_error}"
                ),
                "remediation": remediation_for("source_binding"),
            })
    blocking_issues = [
        {
            "path": check.spec.path,
            "category": check.spec.category,
            "kind": issue.kind,
            "detail": issue.detail,
            "remediation": remediation_for(issue.kind),
        }
        for check in checks
        for issue in check.issues
        if issue.blocking
    ]
    blocking_issues.extend(readme_blocking_issues)
    operator_explanations = build_operator_explanations(
        blocking_issues=blocking_issues,
        stale_claims=stale_claims,
        redundant_agent_work=redundant_agent_work,
    )

    blocking_count = len(blocking_issues)
    overall_status = "blocked" if blocking_issues else "ready"

    return {
        "schema": REPORT_SCHEMA,
        "generated_at": format_datetime(now),
        "repo_root": str(repo_root),
        "max_age_days": max_age_days,
        "overall_status": overall_status,
        "overall_ready": overall_status == "ready",
        "blocking_issue_count": blocking_count,
        "blocking_count": blocking_count,
        "categories": category_summary(checks),
        "artifacts": [check.to_json() for check in checks],
        "stale_claims": stale_claims,
        "redundant_agent_work": redundant_agent_work,
        "hostcall_queue_telemetry": hostcall_queue_telemetry,
        "operator_explanations": operator_explanations,
        "blocking_issues": blocking_issues,
    }


def print_text_report(report: dict[str, Any]) -> None:
    print(f"schema: {report['schema']}")
    print(f"generated_at: {report['generated_at']}")
    print(f"overall_status: {report['overall_status']}")
    print(f"overall_ready: {str(report['overall_ready']).lower()}")
    print(f"blocking_issue_count: {report['blocking_issue_count']}")
    print(f"blocking_count: {report['blocking_count']}")
    print("")
    print("categories:")
    for category in report["categories"]:
        print(
            f"  {category['category']}: {category['status']} "
            f"({category['blocking_issues']} blocking)"
        )
    print("")
    print("artifacts:")
    for artifact in report["artifacts"]:
        age = artifact["age_days"]
        age_text = "n/a" if age is None else f"{age}d"
        print(
            f"  {artifact['status']}: {artifact['path']} "
            f"[{artifact['category']}, {artifact['claim_surface']}, age={age_text}]"
        )
        for issue in artifact["issues"]:
            marker = "BLOCK" if issue["blocking"] else "INFO"
            print(f"    {marker} {issue['kind']}: {issue['detail']}")
    stale_claims = report["stale_claims"]
    print("")
    print("stale in-progress claims:")
    print(
        f"  {stale_claims['status']}: "
        f"{stale_claims['summary']['in_progress_count']} in_progress, "
        f"{stale_claims['summary']['stale_count']} stale, "
        f"{stale_claims['summary']['ambiguous_count']} ambiguous, "
        f"{stale_claims['summary']['missing_evidence_count']} missing evidence"
    )
    for item in stale_claims["items"]:
        print(
            f"  {item['classification']}: {item['bead_id']} "
            f"assignee={item['assignee'] or 'none'} "
            f"last_update={item['last_update'] or 'unknown'} "
            f"source={item['evidence_source']}"
        )
        if item["dirty_worktree_paths"]:
            print(f"    dirty: {', '.join(item['dirty_worktree_paths'])}")
        print(f"    action: {item['recommended_operator_action']}")
    redundant_work = report["redundant_agent_work"]
    print("")
    print("redundant agent work:")
    print(
        f"  {redundant_work['status']}: "
        f"{redundant_work['summary']['risk_group_count']} risk groups, "
        f"{redundant_work['summary']['high_confidence_group_count']} high confidence, "
        f"coordination={redundant_work['coordination']['classification']}"
    )
    for group in redundant_work["groups"]:
        member_ids = ", ".join(
            str(member.get("bead_id") or "unknown")
            for member in group.get("members", [])
        )
        print(
            f"  {group['risk']}: {member_ids} "
            f"confidence={group['confidence']} reason={group['match_reason']}"
        )
        if group["shared_surface_paths"]:
            print(f"    shared: {', '.join(group['shared_surface_paths'])}")
        print(f"    action: {group['recommended_operator_action']}")
    hostcall_queue = report["hostcall_queue_telemetry"]
    print("")
    print("hostcall queue telemetry:")
    print(
        f"  {hostcall_queue['status']}: "
        f"{hostcall_queue['summary']['sources_with_hostcall_queue_telemetry']}/"
        f"{hostcall_queue['summary']['sources_checked']} sources with telemetry, "
        f"{hostcall_queue['summary']['missing_required_field_count']} missing required fields"
    )
    summary = hostcall_queue["summary"]
    print(
        "  totals: "
        f"s3fifo_fallback_transitions={summary['s3fifo_fallback_transitions']}, "
        f"s3fifo_fairness_rejected_total={summary['s3fifo_fairness_rejected_total']}, "
        f"s3fifo_lane_overflow_rejected_total={summary['s3fifo_lane_overflow_rejected_total']}, "
        f"bravo_rollbacks_total={summary['bravo_rollbacks_total']}"
    )
    for source in hostcall_queue["sources"]:
        metrics = source["metrics"]
        print(
            f"  {source['status']}: {source['path']} "
            f"reactor={source['reactor_path'] or 'missing'}"
        )
        print(
            "    "
            f"s3fifo_fallback_transitions={metrics['s3fifo_fallback_transitions']['value']!r}, "
            f"s3fifo_fairness_rejected_total="
            f"{metrics['s3fifo_fairness_rejected_total']['value']!r}, "
            f"s3fifo_lane_overflow_rejected_total="
            f"{metrics['s3fifo_lane_overflow_rejected_total']['value']!r}, "
            f"bravo_transitions_total={metrics['bravo_transitions_total']['value']!r}, "
            f"bravo_rollbacks_total={metrics['bravo_rollbacks_total']['value']!r}"
        )
        if source["missing_required_fields"]:
            print(f"    missing: {', '.join(source['missing_required_fields'])}")
        print(f"    action: {source['recommended_operator_action']}")
    operator_explanations = report["operator_explanations"]
    print("")
    print("operator explanations:")
    print(
        f"  {operator_explanations['status']}: "
        f"{operator_explanations['summary']['explanation_count']} explanations"
    )
    for item in operator_explanations["items"]:
        print(f"  {item['category']}: {item['affected']}")
        print(f"    explanation: {item['explanation']}")
        print(f"    next: {item['next_safe_action']}")
    if report["blocking_issues"]:
        print("")
        print("release-facing claim blockers:")
        for issue in report["blocking_issues"]:
            location = issue["path"]
            if issue.get("line") is not None:
                location = f"{location}:{issue['line']}"
            print(f"  {location}: {issue['kind']}: {issue['detail']}")
            print(f"    remediation: {issue['remediation']}")


def write_artifact(
    repo_root: Path,
    relative_path: str,
    payload: dict[str, Any] | None,
    *,
    text: str | None = None,
    mtime: datetime | None = None,
) -> None:
    full_path = repo_root / relative_path
    full_path.parent.mkdir(parents=True, exist_ok=True)
    if payload is not None:
        full_path.write_text(json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8")
    else:
        assert text is not None
        full_path.write_text(text, encoding="utf-8")
    if mtime is not None:
        ts = mtime.timestamp()
        os.utime(full_path, (ts, ts))


def fixture_payload(
    spec: EvidenceSpec,
    now: datetime,
    provenance: str,
) -> dict[str, Any] | None:
    if spec.path.endswith(".md"):
        return None

    payload: dict[str, Any] = {
        "generated_at": format_datetime(now),
        "correlation_id": provenance,
    }
    if spec.required_schema is not None:
        payload["schema"] = spec.required_schema
    if spec.status_path is not None:
        ok_value = spec.ok_values[0] if spec.ok_values else "pass"
        assign_path(payload, spec.status_path, ok_value)
    for zero_path in spec.zero_paths:
        assign_path(payload, zero_path, 0)
    for required_true_path in spec.required_true_paths:
        assign_path(payload, required_true_path, True)
    for required_empty_list_path in spec.required_empty_list_paths:
        assign_path(payload, required_empty_list_path, [])
    if spec.id == "perf_budget_summary":
        canonical_summary_path = (
            Path(__file__).resolve().parent.parent / spec.path
        )
        canonical_summary, canonical_error = load_json(canonical_summary_path)
        if canonical_error is not None or canonical_summary is None:
            raise AssertionError(
                "failed to load canonical performance budget inventory: "
                f"{canonical_error}"
            )
        raw_budgets = canonical_summary.get("budgets")
        if not isinstance(raw_budgets, list) or not raw_budgets:
            raise AssertionError("canonical performance budget inventory is empty")
        budgets = json.loads(json.dumps(raw_budgets))
        budget_results = [
            {
                "budget_name": budget["name"],
                "category": budget["category"],
                "threshold": budget["threshold"],
                "comparison": budget["comparison"],
                "unit": budget["unit"],
                "actual": budget["threshold"],
                "status": "PASS",
                "source": f"fixture:{budget['name']}",
                "ci_enforced": budget["ci_enforced"],
            }
            for budget in budgets
        ]
        ci_enforced = sum(
            int(budget["ci_enforced"] is True) for budget in budgets
        )
        payload.update({
            "generated_at": now.isoformat(timespec="milliseconds").replace("+00:00", "Z"),
            "run_id": provenance,
            "source_commit": "0123456789abcdef0123456789abcdef01234567",
            "total_budgets": len(budgets),
            "pass": len(budgets),
            "fail": 0,
            "no_data": 0,
            "ci_enforced": ci_enforced,
            "ci_with_data": ci_enforced,
            "budgets": budgets,
            "budget_results": budget_results,
            "failing_data_contracts": [],
        })
    if spec.id == "dropin_certification_verdict":
        payload["overall_verdict"] = "CERTIFIED"
        payload["generated_at_utc"] = format_datetime(now)
        payload["hard_gate_results"] = [
            {"gate_id": f"blocking_{index}", "status": "pass", "blocking": True}
            for index in range(14)
        ] + [
            {"gate_id": f"advisory_{index}", "status": "pass", "blocking": False}
            for index in range(6)
        ]
    if spec.id in {"full_suite_verdict", "certification_lane"}:
        payload["summary"] = {
            "total_gates": 20,
            "passed": 20,
            "failed": 0,
            "warned": 0,
            "skipped": 0,
            "blocking_pass": 14,
            "blocking_total": 14,
            "all_blocking_pass": True,
        }
    if spec.id == "evidence_bundle_index":
        payload["ci_run_id"] = provenance
        payload["summary"] = {
            "total_sections": 29,
            "present_sections": 29,
            "missing_sections": 0,
            "invalid_sections": 0,
            "verdict": "complete",
        }
    if spec.id == "extension_must_pass_gate":
        payload["run_id"] = provenance
        payload["observed"] = {
            "must_pass_total": 123,
            "must_pass_tested": 123,
            "must_pass_passed": 123,
            "must_pass_failed": 0,
            "must_pass_skipped": 0,
            "must_pass_pass_rate_pct": 100.0,
            "stretch_total": 101,
            "stretch_tested": 101,
            "stretch_passed": 101,
            "stretch_failed": 0,
            "stretch_skipped": 0,
        }
    if spec.id == "dropin_contract":
        payload.pop("generated_at", None)
        payload["effective_date_utc"] = format_datetime(now)
        payload["status"] = "active_blocking_policy"
    if spec.id == "perf_stress_triage":
        assign_path(payload, "results.reactor.enabled", True)
        assign_path(payload, "results.reactor.rejected_enqueues", 0)
        assign_path(payload, "results.reactor.s3fifo.fairness_budget_rejections", 0)
        assign_path(payload, "results.reactor.s3fifo.lane_overflow_rejections", 0)
        assign_path(payload, "results.reactor.s3fifo.fallback_event_total", 0)
        assign_path(payload, "results.reactor.bravo.mode", "Balanced")
        assign_path(payload, "results.reactor.bravo.transitions", 0)
        assign_path(payload, "results.reactor.bravo.rollbacks", 0)
    if spec.id == "extension_reactor_queue_coverage":
        assign_path(payload, "captured_report_metrics.reactor.enabled", True)
        assign_path(payload, "captured_report_metrics.reactor.rejected_enqueues", 0)
        assign_path(
            payload,
            "captured_report_metrics.reactor.s3fifo.fairness_budget_rejections",
            0,
        )
        assign_path(
            payload,
            "captured_report_metrics.reactor.s3fifo.lane_overflow_rejections",
            0,
        )
        assign_path(payload, "captured_report_metrics.reactor.s3fifo.fallback_event_total", 0)
        assign_path(payload, "captured_report_metrics.reactor.bravo.mode", "Balanced")
        assign_path(payload, "captured_report_metrics.reactor.bravo.transitions", 0)
        assign_path(payload, "captured_report_metrics.reactor.bravo.rollbacks", 0)
    return payload


def assign_path(payload: dict[str, Any], path: str, value: Any) -> None:
    current = payload
    parts = path.split(".")
    for part in parts[:-1]:
        next_value = current.get(part)
        if not isinstance(next_value, dict):
            next_value = {}
            current[part] = next_value
        current = next_value
    current[parts[-1]] = value


def make_complete_fixture(
    repo_root: Path,
    now: datetime,
    provenance: str = "fixture-run",
    skip_ids: set[str] | None = None,
) -> None:
    skip_ids = skip_ids or set()
    for spec in EVIDENCE_SPECS:
        if spec.id in skip_ids:
            continue
        payload = fixture_payload(spec, now, provenance)
        if payload is None:
            write_artifact(repo_root, spec.path, None, text=f"# {spec.id}\n", mtime=now)
        else:
            write_artifact(repo_root, spec.path, payload, mtime=now)
    write_readme_release_snapshot_fixture(repo_root, now, provenance)
    write_artifact(
        repo_root,
        "Cargo.toml",
        None,
        text=(
            "[package]\n"
            'name = "pi-claim-readiness-fixture"\n'
            'version = "0.0.0"\n'
            'edition = "2024"\n'
            'include = ["Cargo.toml"]\n'
        ),
        mtime=now,
    )

    commit_fixture_changes(repo_root, "fixture source snapshot")

    source_commit = git_output(repo_root, ["rev-parse", "HEAD"])
    if source_commit is None:
        raise AssertionError("fixture repository has no source commit")
    source_commit = source_commit.strip()
    budget_path = repo_root / EVIDENCE_SPECS[0].path
    budget_payload, budget_error = load_json(budget_path)
    if budget_error is not None or budget_payload is None:
        raise AssertionError(f"failed to bind fixture budget summary: {budget_error}")
    budget_payload["source_commit"] = source_commit
    write_artifact(repo_root, EVIDENCE_SPECS[0].path, budget_payload)
    commit_fixture_changes(repo_root, "bind performance evidence to source")


def commit_fixture_changes(repo_root: Path, message: str) -> None:
    """Commit all current fixture changes with an isolated test identity."""
    commands = (
        ("add", "--all"),
        ("add", "--renormalize", "--", "."),
        (
            "-c",
            "user.name=Pi claim-readiness fixture",
            "-c",
            "user.email=pi-fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            message,
        ),
    )
    for args in commands:
        result = subprocess.run(
            ["git", "-C", str(repo_root), *args],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
        if result.returncode != 0:
            raise AssertionError(
                f"fixture git {' '.join(args)} failed: {result.stderr.strip()}"
            )


def rebind_fixture_performance_summary(repo_root: Path) -> None:
    """Commit a new source snapshot, then bind and commit its evidence summary."""
    commit_fixture_changes(repo_root, "update fixture source snapshot")
    source_commit = git_output(repo_root, ["rev-parse", "HEAD"])
    if source_commit is None:
        raise AssertionError("fixture repository has no updated source commit")
    budget_path = repo_root / EVIDENCE_SPECS[0].path
    budget_payload, budget_error = load_json(budget_path)
    if budget_error is not None or budget_payload is None:
        raise AssertionError(f"failed to rebind fixture budget summary: {budget_error}")
    budget_payload["source_commit"] = source_commit.strip()
    write_artifact(repo_root, EVIDENCE_SPECS[0].path, budget_payload)
    commit_fixture_changes(repo_root, "rebind performance evidence to updated source")


def write_readme_release_snapshot_fixture(repo_root: Path, now: datetime, provenance: str) -> None:
    generated_at = format_datetime(now)
    assert generated_at is not None
    date = generated_at[:10]
    text = f"""# Fixture README

### Latest run snapshot (extension gate refresh {date})

From:
- `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json` (generated `{generated_at}`, run `{provenance}`)
- `tests/ext_conformance/reports/health_delta/health_delta_report.json` (generated `{generated_at}`)
- `tests/ext_conformance/reports/journeys/journey_report.json` (generated `{generated_at}`)
- `tests/evidence_bundle/index.json` (generated `{generated_at}`, run `{provenance}`)
- `tests/full_suite_gate/certification_verdict.json` (generated `{generated_at}`)
- `docs/evidence/dropin-certification-verdict.json` (generated `{generated_at}`)

- Strict drop-in status: **20/20 certification gates PASS, 14/14 blocking gates PASS** - `CERTIFIED`
- Unified evidence bundle: `29/29` sections present, `0` missing, `0` invalid
- Extension must-pass gate: `123/123` must-pass extensions passed; informational stretch set `101/101` passed

## Installation

Latest certification/evidence refresh (`{date}` extension gate and full-suite reports; `{date}` drop-in certification verdict):
- Unified evidence bundle: `29/29` sections present, `0` missing, `0` invalid
- Full-suite gate: `20/20` gates passed, including `14/14` blocking gates
- Drop-in certification: `20/20` certification gates passed, overall verdict `CERTIFIED`
- Extension must-pass gate: `123/123` must-pass extensions passed; stretch set `101/101` passed

### Fast Loop
"""
    write_artifact(repo_root, "README.md", None, text=text, mtime=now)


def write_beads_ledger(repo_root: Path, issues: list[dict[str, Any]]) -> None:
    ledger_path = repo_root / ".beads" / "issues.jsonl"
    ledger_path.parent.mkdir(parents=True, exist_ok=True)
    with ledger_path.open("w", encoding="utf-8") as handle:
        for issue in issues:
            handle.write(json.dumps(issue, sort_keys=True))
            handle.write("\n")


def write_activity_jsonl(
    repo_root: Path,
    relative_path: str,
    rows: list[dict[str, Any]],
) -> None:
    full_path = repo_root / relative_path
    full_path.parent.mkdir(parents=True, exist_ok=True)
    with full_path.open("w", encoding="utf-8") as handle:
        for row in rows:
            handle.write(json.dumps(row, sort_keys=True))
            handle.write("\n")


def stale_claim_item(report: dict[str, Any], bead_id: str) -> dict[str, Any]:
    for item in report["stale_claims"]["items"]:
        if item["bead_id"] == bead_id:
            return item
    raise AssertionError(f"missing stale claim item for {bead_id}")


def hostcall_source(report: dict[str, Any], source_id: str) -> dict[str, Any]:
    for source in report["hostcall_queue_telemetry"]["sources"]:
        if source["id"] == source_id:
            return source
    raise AssertionError(f"missing hostcall queue source for {source_id}")


def operator_categories(report: dict[str, Any]) -> set[str]:
    return {
        item["category"]
        for item in report["operator_explanations"]["items"]
    }


def assert_condition(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def repo_golden_path(golden_name: str) -> tuple[Path, Path]:
    relative_path = GOLDEN_REPORT_DIRECTORY / golden_name
    return Path(__file__).resolve().parent.parent / relative_path, relative_path


def canonical_report_projection(report: dict[str, Any]) -> dict[str, Any]:
    """Keep the golden focused on stable operator-facing report structure."""

    return {
        "schema": report["schema"],
        "generated_at": report["generated_at"],
        "max_age_days": report["max_age_days"],
        "overall_status": report["overall_status"],
        "overall_ready": report["overall_ready"],
        "blocking_issue_count": report["blocking_issue_count"],
        "blocking_count": report["blocking_count"],
        "categories": report["categories"],
        "artifact_statuses": [
            {
                "id": artifact["id"],
                "category": artifact["category"],
                "claim_surface": artifact["claim_surface"],
                "release_blocking": artifact["release_blocking"],
                "status": artifact["status"],
                "exists": artifact["exists"],
                "schema": artifact["schema"],
                "issue_count": len(artifact["issues"]),
                "issue_kinds": [issue["kind"] for issue in artifact["issues"]],
            }
            for artifact in report["artifacts"]
        ],
        "stale_claims": report["stale_claims"],
        "hostcall_queue_telemetry": report["hostcall_queue_telemetry"],
        "operator_explanations": report["operator_explanations"],
    }


def stable_json(payload: dict[str, Any]) -> str:
    return json.dumps(payload, indent=2, sort_keys=True) + "\n"


def assert_report_matches_golden(
    report: dict[str, Any],
    golden_name: str = COMPLETE_REPORT_GOLDEN,
) -> None:
    actual = stable_json(canonical_report_projection(report))
    golden_path, relative_path = repo_golden_path(golden_name)
    if os.environ.get(UPDATE_GOLDEN_ENV) == "1":
        golden_path.parent.mkdir(parents=True, exist_ok=True)
        golden_path.write_text(actual, encoding="utf-8")
        return

    try:
        expected = golden_path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise AssertionError(
            f"{relative_path} is missing; run "
            f"`{UPDATE_GOLDEN_ENV}=1 python3 scripts/report_swarm_claim_readiness.py "
            "--self-test` to create it, then review the diff."
        ) from exc

    if actual != expected:
        diff = "".join(
            difflib.unified_diff(
                expected.splitlines(keepends=True),
                actual.splitlines(keepends=True),
                fromfile=str(relative_path),
                tofile="actual swarm claim readiness projection",
            )
        )
        raise AssertionError(
            "swarm claim readiness JSON projection changed; update the golden only "
            f"after review with `{UPDATE_GOLDEN_ENV}=1 python3 "
            "scripts/report_swarm_claim_readiness.py --self-test`\n"
            f"{diff}"
        )


def run_self_test() -> int:
    now = datetime(2026, 5, 8, 12, 0, 0, tzinfo=timezone.utc)
    stale = now - timedelta(days=30)

    try:
        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(repo_root, [])
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        assert_condition(report["overall_status"] == "ready", "fresh fixture should be ready")
        assert_condition(report["overall_ready"] is True, "overall_ready should mirror ready status")
        assert_condition(
            report["blocking_count"] == report["blocking_issue_count"],
            "blocking_count should mirror blocking_issue_count",
        )
        assert_report_matches_golden(report)
        hostcall = report["hostcall_queue_telemetry"]
        assert_condition(
            hostcall["status"] == "ready",
            "complete fixture hostcall telemetry should be ready",
        )
        stress_source = hostcall_source(report, "perf_stress_triage")
        assert_condition(
            stress_source["metrics"]["s3fifo_fairness_rejected_total"]["present"],
            "S3-FIFO fairness rejection counter should be present",
        )
        assert_condition(
            stress_source["metrics"]["bravo_rollbacks_total"]["present"],
            "BRAVO rollback counter should be present",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        readme_path = repo_root / "README.md"
        readme_path.write_text(
            readme_path.read_text(encoding="utf-8").replace("101/101", "100/101", 1),
            encoding="utf-8",
        )
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        readme_blockers = [
            issue
            for issue in report["blocking_issues"]
            if issue["kind"] == "readme_snapshot_mismatch"
        ]
        assert_condition(
            readme_blockers,
            "stale README release snapshot counts should block claim readiness",
        )
        assert_condition(
            readme_blockers[0].get("line") is not None,
            "README snapshot mismatch should include an exact line number",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        readme_path = repo_root / "README.md"
        readme_path.write_text(
            readme_path.read_text(encoding="utf-8").replace(
                "Full-suite gate: `20/20` gates passed",
                "Full-suite gate: `19/20` gates passed",
                1,
            ),
            encoding="utf-8",
        )
        rebind_fixture_performance_summary(repo_root)
        full_suite_spec = next(
            spec for spec in EVIDENCE_SPECS if spec.id == "full_suite_verdict"
        )
        full_suite_path = repo_root / full_suite_spec.path
        full_suite_contents = full_suite_path.read_bytes()
        full_suite_metadata = full_suite_path.stat()
        transient_full_suite, transient_full_suite_error = load_json_bytes(
            full_suite_contents
        )
        assert_condition(
            transient_full_suite_error is None and transient_full_suite is not None,
            "full-suite mutation fixture must start with valid JSON",
        )
        assert transient_full_suite is not None
        transient_summary = transient_full_suite.get("summary")
        assert_condition(
            isinstance(transient_summary, dict),
            "full-suite mutation fixture must contain a summary object",
        )
        assert isinstance(transient_summary, dict)
        transient_summary["passed"] = 19
        transient_summary["failed"] = 1
        transient_full_suite_contents = (
            json.dumps(transient_full_suite, sort_keys=True) + "\n"
        ).encode("utf-8")
        assert_condition(
            readme_gate_counts(transient_full_suite) == (20, 19, 14, 14),
            "transient full-suite fixture must agree with the forged README counts",
        )
        mutated_during_readme_expectations = False
        restored_after_readme_expectations = False

        def mutate_full_suite_during_readme_expectations() -> None:
            nonlocal mutated_during_readme_expectations
            full_suite_path.write_bytes(transient_full_suite_contents)
            mutated_during_readme_expectations = True

        def restore_full_suite_after_readme_expectations() -> None:
            nonlocal restored_after_readme_expectations
            full_suite_path.write_bytes(full_suite_contents)
            os.utime(
                full_suite_path,
                ns=(
                    full_suite_metadata.st_atime_ns,
                    full_suite_metadata.st_mtime_ns,
                ),
            )
            restored_after_readme_expectations = True

        report = build_report(
            repo_root,
            now=now,
            _before_readme_expectations=(
                mutate_full_suite_during_readme_expectations
            ),
            _after_readme_expectations=(
                restore_full_suite_after_readme_expectations
            ),
        )
        assert_condition(
            mutated_during_readme_expectations,
            "README expectation mutation hook did not execute",
        )
        assert_condition(
            restored_after_readme_expectations,
            "README expectation restoration hook did not execute",
        )
        assert_condition(
            full_suite_path.read_bytes() == full_suite_contents,
            "README expectation fixture did not restore the evidence bytes",
        )
        readme_blockers = [
            issue
            for issue in report["blocking_issues"]
            if issue["kind"] == "readme_snapshot_mismatch"
            and "refresh_full_suite_counts" in issue["detail"]
        ]
        assert_condition(
            readme_blockers,
            "transient artifact mutation must not suppress captured README expectations",
        )
        full_suite_artifact = next(
            artifact
            for artifact in report["artifacts"]
            if artifact["id"] == full_suite_spec.id
        )
        assert_condition(
            full_suite_artifact["status"] == "ready",
            "restored evidence must still pass final snapshot revalidation",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        readme_path = repo_root / "README.md"
        readme_path.write_text(
            readme_path.read_text(encoding="utf-8").replace("`CERTIFIED`", "`NOT_CERTIFIED`", 1),
            encoding="utf-8",
        )
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        readme_blockers = [
            issue
            for issue in report["blocking_issues"]
            if issue["kind"] == "readme_snapshot_mismatch"
            and "latest_dropin_verdict" in issue["detail"]
        ]
        assert_condition(
            readme_blockers,
            "stale README drop-in verdict should block claim readiness",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        readme_path = repo_root / "README.md"
        readme_path.write_text(
            readme_path.read_text(encoding="utf-8").replace(
                "tests/ext_conformance/reports/journeys/journey_report.json",
                "tests/ext_conformance/reports/journeys/missing_journey_report.json",
                1,
            ),
            encoding="utf-8",
        )
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        readme_blockers = [
            issue
            for issue in report["blocking_issues"]
            if issue["kind"] == "readme_snapshot_missing"
        ]
        assert_condition(
            any("missing_journey_report.json" in issue["detail"] for issue in readme_blockers),
            "missing README snapshot artifact citation should block claim readiness",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        stale_generated = format_datetime(now - timedelta(days=1))
        assert stale_generated is not None
        health_delta_path = repo_root / "tests/ext_conformance/reports/health_delta/health_delta_report.json"
        health_delta = json.loads(health_delta_path.read_text(encoding="utf-8"))
        health_delta["generated_at"] = stale_generated
        health_delta_path.write_text(json.dumps(health_delta, sort_keys=True) + "\n", encoding="utf-8")
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        readme_blockers = [
            issue
            for issue in report["blocking_issues"]
            if issue["kind"] == "readme_snapshot_mismatch"
        ]
        assert_condition(
            any("health_delta_report.json" in issue["detail"] for issue in readme_blockers),
            "README snapshot generated_at drift should block claim readiness",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        payload = fixture_payload(EVIDENCE_SPECS[0], stale, "fixture-run")
        assert payload is not None
        write_artifact(repo_root, EVIDENCE_SPECS[0].path, payload, mtime=stale)
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        kinds = {issue["kind"] for issue in report["blocking_issues"]}
        assert_condition("stale" in kinds, "stale artifact should block gate mode")
        assert_condition(
            "stale_evidence" in operator_categories(report),
            "stale artifact blockers should have operator explanations",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        policy_spec = next(spec for spec in EVIDENCE_SPECS if spec.id == "dropin_contract")
        policy_payload = fixture_payload(policy_spec, now, "fixture-run")
        assert policy_payload is not None
        policy_payload["effective_date_utc"] = format_datetime(stale)
        write_artifact(repo_root, policy_spec.path, policy_payload, mtime=stale)
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        policy_artifact = next(
            artifact for artifact in report["artifacts"] if artifact["id"] == "dropin_contract"
        )
        assert_condition(
            policy_artifact["status"] == "release_policy",
            "old release-policy contract dates should stay classified as policy, not stale evidence",
        )
        assert_condition(
            not policy_artifact["issues"],
            "old release-policy contract dates should not create stale evidence issues",
        )
        assert_condition(
            report["overall_status"] == "ready",
            "old release-policy contract dates should not block claim readiness",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now, skip_ids={"activity_ledger_digest"})
        report = build_report(repo_root, now=now)
        paths = {issue["path"] for issue in report["blocking_issues"]}
        assert_condition(
            "tests/full_suite_gate/swarm_activity_digest.json" in paths,
            "missing artifact should be reported with exact path",
        )
        assert_condition(
            "missing_artifact" in operator_categories(report),
            "missing artifact blockers should have operator explanations",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        budget_path = "tests/perf/reports/budget_summary.json"
        payload = fixture_payload(EVIDENCE_SPECS[0], now, "fixture-run")
        assert payload is not None
        duplicate_key_text = json.dumps(payload, sort_keys=True).replace(
            "{",
            f'{{"schema":{json.dumps(payload["schema"])},',
            1,
        )
        (repo_root / budget_path).write_text(
            duplicate_key_text + "\n",
            encoding="utf-8",
        )
        report = build_report(repo_root, now=now)
        budget_artifact = next(
            artifact
            for artifact in report["artifacts"]
            if artifact["id"] == "perf_budget_summary"
        )
        assert_condition(
            budget_artifact["status"] == "invalid_json",
            "duplicate-key performance summaries must be invalid JSON",
        )
        assert_condition(
            any(
                "duplicate JSON object key: schema" in issue["detail"]
                for issue in budget_artifact["issues"]
            ),
            "duplicate-key performance summaries must report the repeated key",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        payload = fixture_payload(EVIDENCE_SPECS[0], now, "fixture-run")
        assert payload is not None
        payload["ci_no_data"] = 2
        write_artifact(repo_root, budget_path, payload, mtime=now)
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition("ci_no_data=2" in details, "no-data budget summary should block")

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        payload = fixture_payload(EVIDENCE_SPECS[0], now, "fixture-run")
        assert payload is not None
        payload["claim_readiness"] = {
            "status": "blocked",
            "performance_claims_authorized": False,
            "blocking_reason_codes": ["ci_budget_data_missing"],
        }
        write_artifact(repo_root, budget_path, payload, mtime=now)
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition(
            "claim_readiness.status='blocked'" in details,
            "blocked v2 claim_readiness status should block",
        )
        assert_condition(
            "performance_claims_authorized=False" in details,
            "false v2 performance claim authorization should block",
        )
        assert_condition(
            "blocking_reason_codes=['ci_budget_data_missing']" in details,
            "non-empty v2 claim-readiness blockers should block",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        payload = fixture_payload(EVIDENCE_SPECS[0], now, "fixture-run")
        assert payload is not None
        payload["schema"] = "pi.perf.budget_summary.v1"
        write_artifact(repo_root, budget_path, payload, mtime=now)
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition(
            "expected schema 'pi.perf.budget_summary.v2'" in details,
            "legacy v1 budget summary must not authorize current claims",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        payload, payload_error = load_json(repo_root / budget_path)
        assert payload_error is None and payload is not None
        payload["generated_at"] = (
            now + timedelta(minutes=6)
        ).isoformat(timespec="milliseconds").replace("+00:00", "Z")
        write_artifact(repo_root, budget_path, payload, mtime=now)
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition(
            "more than five minutes in the future" in details,
            "future-dated v2 budget summary must fail closed",
        )

        payload["generated_at"] = now.isoformat(timespec="milliseconds").replace("+00:00", "Z")
        payload["source_commit"] = "fedcba9876543210fedcba9876543210fedcba98"
        write_artifact(repo_root, budget_path, payload, mtime=now)
        commit_fixture_changes(repo_root, "record forged source binding fixture")
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition(
            "could not be resolved" in details,
            "forged v2 source_commit must fail closed",
        )

        bound_source_commit = git_output(repo_root, ["rev-parse", "HEAD"])
        assert bound_source_commit is not None
        payload["source_commit"] = bound_source_commit.strip()
        non_ci_result = next(
            result
            for result in payload["budget_results"]
            if result["ci_enforced"] is False
        )
        non_ci_result["actual"] = None
        non_ci_result["status"] = "NO_DATA"
        payload["pass"] = payload["total_budgets"] - 1
        payload["no_data"] = 1
        payload["claim_readiness"] = {
            "status": "blocked",
            "performance_claims_authorized": False,
            "blocking_reason_codes": ["budget_data_missing"],
        }
        write_artifact(repo_root, budget_path, payload, mtime=now)
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition(
            "blocking_reason_codes=['budget_data_missing']" in details,
            "global no-data must block even when the CI subset is green",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        payload, payload_error = load_json(repo_root / budget_path)
        assert payload_error is None and payload is not None
        payload.pop("budget_results")
        write_artifact(repo_root, budget_path, payload, mtime=now)
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition(
            "budget_results" in details and "missing" in details,
            "aggregate-only performance summaries must fail closed",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_artifact(
            repo_root,
            "untracked-release-input.txt",
            None,
            text="untracked source mutation\n",
        )
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition(
            "repository is not clean" in details,
            "untracked source mutations must invalidate performance claims",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        cargo_path = repo_root / "Cargo.toml"
        cargo_path.write_text(
            cargo_path.read_text(encoding="utf-8") + "\n# staged mutation\n",
            encoding="utf-8",
        )
        staged = subprocess.run(
            ["git", "-C", str(repo_root), "add", "--", "Cargo.toml"],
            check=False,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert_condition(staged.returncode == 0, "failed to stage source mutation fixture")
        report = build_report(repo_root, now=now)
        details = "\n".join(issue["detail"] for issue in report["blocking_issues"])
        assert_condition(
            "repository is not clean" in details,
            "staged source mutations must invalidate performance claims",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        head_advanced = False

        def advance_head_between_evidence_checks(index: int) -> None:
            nonlocal head_advanced
            if index != 0 or head_advanced:
                return
            write_artifact(
                repo_root,
                "tests/perf/reports/report_head_advance.json",
                {
                    "schema": "pi.swarm.report_head_advance_fixture.v1",
                    "generated_at": format_datetime(now),
                },
                mtime=now,
            )
            commit_fixture_changes(
                repo_root,
                "advance HEAD between claim-readiness evidence checks",
            )
            head_advanced = True

        report = build_report(
            repo_root,
            now=now,
            _after_evidence_check=advance_head_between_evidence_checks,
        )
        source_binding_details = "\n".join(
            issue["detail"]
            for issue in report["blocking_issues"]
            if issue["kind"] == "source_binding"
        )
        assert_condition(head_advanced, "HEAD-advance fixture hook did not execute")
        assert_condition(
            "different repository HEAD" in source_binding_details
            or "repository changed while the claim-readiness report was being assembled"
            in source_binding_details,
            "one report must not combine release evidence captured from different commits",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        late_mutation_spec = next(
            spec
            for spec in reversed(EVIDENCE_SPECS)
            if spec.claim_surface == "release_facing"
        )
        mutated_after_check = False

        def mutate_already_checked_artifact() -> None:
            nonlocal mutated_after_check
            if mutated_after_check:
                return
            artifact_path = repo_root / late_mutation_spec.path
            artifact_path.write_bytes(artifact_path.read_bytes() + b"\n")
            mutated_after_check = True

        report = build_report(
            repo_root,
            now=now,
            _before_final_binding_check=mutate_already_checked_artifact,
        )
        source_binding_details = "\n".join(
            issue["detail"]
            for issue in report["blocking_issues"]
            if issue["kind"] == "source_binding"
        )
        assert_condition(
            mutated_after_check,
            "post-check artifact-mutation fixture hook did not execute",
        )
        assert_condition(
            "changed after its evidence check" in source_binding_details,
            "one report must revalidate every captured artifact after all checks finish",
        )
        mutated_artifact = next(
            artifact
            for artifact in report["artifacts"]
            if artifact["id"] == late_mutation_spec.id
        )
        assert_condition(
            mutated_artifact["status"] == "source_binding"
            and any(
                issue["kind"] == "source_binding"
                for issue in mutated_artifact["issues"]
            ),
            "late source-binding failures must appear on the affected artifact",
        )
        mutated_category = next(
            category
            for category in report["categories"]
            if category["category"] == late_mutation_spec.category
        )
        assert_condition(
            mutated_category["status"] == "blocked"
            and mutated_category["blocking_issues"] >= 1,
            "late source-binding failures must block the affected category summary",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        activity_digest_spec = next(
            spec for spec in EVIDENCE_SPECS if spec.id == "activity_ledger_digest"
        )
        payload, payload_error = load_json(repo_root / activity_digest_spec.path)
        assert payload_error is None and payload is not None
        payload["generated_at"] = format_datetime(now + timedelta(minutes=6))
        write_artifact(repo_root, activity_digest_spec.path, payload, mtime=now)
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        activity_digest_artifact = next(
            artifact
            for artifact in report["artifacts"]
            if artifact["id"] == activity_digest_spec.id
        )
        assert_condition(
            activity_digest_artifact["status"] == "future_timestamp",
            "future-dated non-performance release evidence must fail closed",
        )
        assert_condition(
            any(
                issue["kind"] == "future_timestamp"
                for issue in activity_digest_artifact["issues"]
            ),
            "future-dated release evidence must report its clock-integrity failure",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        activity_digest_path = repo_root / activity_digest_spec.path
        symlink_target = activity_digest_path.with_name("activity_digest_target.json")
        activity_digest_path.rename(symlink_target)
        activity_digest_path.symlink_to(symlink_target.name)
        report = build_report(repo_root, now=now)
        activity_digest_artifact = next(
            artifact
            for artifact in report["artifacts"]
            if artifact["id"] == activity_digest_spec.id
        )
        assert_condition(
            any(
                issue["kind"] == "source_binding"
                and "symlink components" in issue["detail"]
                for issue in activity_digest_artifact["issues"]
            ),
            "release-facing evidence reached through a symlink must fail source binding",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        activity_digest_path = repo_root / activity_digest_spec.path
        original_activity_bytes = activity_digest_path.read_bytes()
        original_capture = globals()["capture_release_artifact_snapshot"]

        def capture_then_substitute(
            fixture_root: Path,
            fixture_path: str,
        ) -> tuple[ReleaseArtifactSnapshot | None, str | None]:
            snapshot, error = original_capture(fixture_root, fixture_path)
            if snapshot is not None and fixture_path == activity_digest_spec.path:
                snapshot.full_path.write_bytes(snapshot.contents + b" ")
            return snapshot, error

        globals()["capture_release_artifact_snapshot"] = capture_then_substitute
        try:
            report = build_report(repo_root, now=now)
        finally:
            globals()["capture_release_artifact_snapshot"] = original_capture
            activity_digest_path.write_bytes(original_activity_bytes)
        activity_digest_artifact = next(
            artifact
            for artifact in report["artifacts"]
            if artifact["id"] == activity_digest_spec.id
        )
        assert_condition(
            any(
                issue["kind"] == "source_binding"
                and "raw bytes changed during validation" in issue["detail"]
                for issue in activity_digest_artifact["issues"]
            ),
            "production report path must reject validate-then-substitute TOCTOU",
        )

        if os.name != "nt":
            repo_root = fixture_root()
            make_complete_fixture(repo_root, now)
            activity_digest_path = repo_root / activity_digest_spec.path
            os.chmod(
                activity_digest_path,
                activity_digest_path.stat().st_mode | 0o100,
            )
            report = build_report(repo_root, now=now)
            activity_digest_artifact = next(
                artifact
                for artifact in report["artifacts"]
                if artifact["id"] == activity_digest_spec.id
            )
            assert_condition(
                any(
                    issue["kind"] == "source_binding"
                    and "executable mode does not exactly match HEAD" in issue["detail"]
                    for issue in activity_digest_artifact["issues"]
                ),
                "production report path must bind live executable mode to HEAD",
            )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        health_delta_spec = next(
            spec for spec in EVIDENCE_SPECS if spec.id == "extension_health_delta"
        )
        payload = fixture_payload(health_delta_spec, now, "fixture-run")
        assert payload is not None
        payload["current_summary"] = {"skipped": 1, "excluded": 1}
        write_artifact(repo_root, health_delta_spec.path, payload, mtime=now)
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        health_delta_blockers = [
            issue
            for issue in report["blocking_issues"]
            if issue["path"] == health_delta_spec.path
        ]
        assert_condition(
            not health_delta_blockers,
            "explicit health-delta exclusions should not count as no-data skips",
        )

        payload["current_summary"] = {"skipped": 2, "excluded": 1}
        write_artifact(repo_root, health_delta_spec.path, payload, mtime=now)
        report = build_report(repo_root, now=now)
        health_delta_details = "\n".join(
            issue["detail"]
            for issue in report["blocking_issues"]
            if issue["path"] == health_delta_spec.path
        )
        assert_condition(
            "unaccounted_skipped=1" in health_delta_details,
            "unaccounted health-delta skips should still block release claims",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        payload = fixture_payload(EVIDENCE_SPECS[1], now, "other-run")
        assert payload is not None
        write_artifact(repo_root, EVIDENCE_SPECS[1].path, payload, mtime=now)
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        kinds = {issue["kind"] for issue in report["blocking_issues"]}
        assert_condition(
            "provenance_mismatch" in kinds,
            "mixed provenance should be reported",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        reactor_spec = next(
            spec for spec in EVIDENCE_SPECS if spec.id == "extension_reactor_queue_coverage"
        )
        payload = fixture_payload(reactor_spec, now, "reactor-run")
        assert payload is not None
        write_artifact(repo_root, reactor_spec.path, payload, mtime=now)
        report = build_report(repo_root, now=now)
        extension_mismatch_paths = {
            issue["path"]
            for issue in report["blocking_issues"]
            if issue["category"] == "extension" and issue["kind"] == "provenance_mismatch"
        }
        assert_condition(
            not extension_mismatch_paths,
            "independent extension evidence lanes should not share provenance group",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        historical_path = "tests/ext_conformance/reports/journeys/journey_report.json"
        write_artifact(repo_root, historical_path, {"note": "old"}, mtime=stale)
        report = build_report(repo_root, now=now)
        blocker_paths = {issue["path"] for issue in report["blocking_issues"]}
        assert_condition(
            historical_path not in blocker_paths,
            "historical snapshots should not block release-facing gate status",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(repo_root, [])
        stress_payload = fixture_payload(EVIDENCE_SPECS[1], now, "fixture-run")
        assert stress_payload is not None
        assign_path(stress_payload, "results.reactor.s3fifo.fairness_budget_rejections", 5)
        assign_path(stress_payload, "results.reactor.s3fifo.lane_overflow_rejections", 2)
        assign_path(stress_payload, "results.reactor.s3fifo.fallback_event_total", 1)
        assign_path(stress_payload, "results.reactor.bravo.rollbacks", 3)
        write_artifact(repo_root, EVIDENCE_SPECS[1].path, stress_payload, mtime=now)
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        hostcall = report["hostcall_queue_telemetry"]
        assert_report_matches_golden(report, "hostcall_fallback_heavy_projection.json")
        assert_condition(
            hostcall["status"] == "fallback_heavy",
            "non-zero hostcall fallback counters should mark the run fallback-heavy",
        )
        assert_condition(
            hostcall["summary"]["s3fifo_fairness_rejected_total"] == 5,
            "hostcall summary should include S3-FIFO fairness rejections",
        )
        assert_condition(
            hostcall["summary"]["bravo_rollbacks_total"] == 3,
            "hostcall summary should include BRAVO rollbacks",
        )
        fallback_source = hostcall_source(report, "perf_stress_triage")
        assert_condition(
            "Inspect S3-FIFO fairness" in fallback_source["recommended_operator_action"],
            "fallback-heavy hostcall telemetry should surface the next safe action",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(repo_root, [])
        stress_payload = fixture_payload(EVIDENCE_SPECS[1], now, "fixture-run")
        assert stress_payload is not None
        stress_payload["results"] = {}
        write_artifact(repo_root, EVIDENCE_SPECS[1].path, stress_payload, mtime=now)
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        assert_report_matches_golden(report, "hostcall_missing_telemetry_projection.json")
        missing_source = hostcall_source(report, "perf_stress_triage")
        assert_condition(
            missing_source["status"] == "missing_telemetry",
            "missing reactor telemetry should be explicit",
        )
        assert_condition(
            "s3fifo_fairness_rejected_total" in missing_source["missing_required_fields"],
            "missing source should list absent S3-FIFO counters",
        )
        assert_condition(
            "bravo_rollbacks_total" in missing_source["missing_required_fields"],
            "missing source should list absent BRAVO counters",
        )
        assert_condition(
            "missing telemetry is reported explicitly"
            in missing_source["recommended_operator_action"],
            "missing hostcall telemetry should explain that absence is not zero pressure",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(repo_root, [])
        stress_payload = fixture_payload(EVIDENCE_SPECS[1], now, "fixture-run")
        assert stress_payload is not None
        stress_payload["results"] = {
            "reactor": {
                "enabled": True,
                "s3fifo": {
                    "fallback_event_total": 0,
                },
            }
        }
        write_artifact(repo_root, EVIDENCE_SPECS[1].path, stress_payload, mtime=now)
        rebind_fixture_performance_summary(repo_root)
        report = build_report(repo_root, now=now)
        assert_report_matches_golden(report, "hostcall_missing_fields_projection.json")
        hostcall = report["hostcall_queue_telemetry"]
        assert_condition(
            hostcall["status"] == "needs_telemetry",
            "partial reactor telemetry should keep the section needs_telemetry",
        )
        missing_source = hostcall_source(report, "perf_stress_triage")
        assert_condition(
            missing_source["status"] == "missing_fields",
            "partial reactor telemetry should be missing_fields, not ready",
        )
        assert_condition(
            missing_source["reactor_path"] == "results.reactor",
            "partial telemetry should preserve the reactor evidence path",
        )
        assert_condition(
            "bravo_rollbacks_total" in missing_source["missing_required_fields"],
            "partial telemetry should list absent BRAVO rollback counter",
        )
        assert_condition(
            "s3fifo_fairness_rejected_total" in missing_source["missing_required_fields"],
            "partial telemetry should list absent S3-FIFO fairness counter",
        )
        assert_condition(
            "Regenerate tests/perf/reports/stress_triage.json"
            in missing_source["recommended_operator_action"],
            "partial hostcall telemetry should surface the exact regeneration action",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_artifact(
            repo_root,
            ".beads/issues.jsonl",
            None,
            text=(
                '{"id":"bd-original","id":"bd-forged",'
                '"status":"in_progress"}\n'
            ),
        )
        report = build_report(repo_root, now=now)
        assert_condition(
            any(
                ".beads/issues.jsonl:1: invalid JSON: "
                "duplicate JSON object key: id" in warning
                for warning in report["stale_claims"]["warnings"]
            ),
            "duplicate-key Beads JSONL rows must be rejected with their source line",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-duplicate-metadata",
                    "status": "in_progress",
                    "updated_at": format_datetime(now),
                    "metadata": (
                        '{"file_paths":["src/original.rs"],'
                        '"file_paths":["src/forged.rs"]}'
                    ),
                }
            ],
        )
        report = build_report(repo_root, now=now)
        assert_condition(
            any(
                "invalid metadata JSON: duplicate JSON object key: file_paths"
                in warning
                for warning in report["stale_claims"]["warnings"]
            ),
            "duplicate-key JSON-encoded metadata must be rejected before path analysis",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        old_update = format_datetime(now - timedelta(hours=30))
        future_timestamp = now + timedelta(minutes=6)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-future-bead",
                    "status": "in_progress",
                    "updated_at": format_datetime(future_timestamp),
                },
                {
                    "id": "bd-future-activity",
                    "status": "in_progress",
                    "updated_at": old_update,
                },
                {
                    "id": "bd-future-commit",
                    "status": "in_progress",
                    "updated_at": old_update,
                },
            ],
        )
        future_activity_path = "tests/full_suite_gate/swarm_activity_events.jsonl"
        write_activity_jsonl(
            repo_root,
            future_activity_path,
            [
                {
                    "bead_id": "bd-future-activity",
                    "timestamp": format_datetime(future_timestamp),
                    "kind": "agent_mail",
                }
            ],
        )
        report = build_report(
            repo_root,
            now=now,
            stale_claim_activity_paths=(future_activity_path,),
            stale_claim_dirty_worktree_paths=(),
            stale_claim_recent_commit_activity={
                "bd-future-commit": ClaimActivityEvidence(
                    bead_id="bd-future-commit",
                    timestamp=future_timestamp,
                    source="git log:future-fixture",
                    agent_name=None,
                )
            },
        )
        for bead_id in (
            "bd-future-bead",
            "bd-future-activity",
            "bd-future-commit",
        ):
            item = stale_claim_item(report, bead_id)
            assert_condition(
                item["classification"] == "invalid_future_timestamp",
                f"{bead_id} must reject materially future timestamps",
            )
            assert_condition(
                item["operator_bucket"] == "ambiguous",
                f"{bead_id} future timestamp must not be classified fresh",
            )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-active",
                    "title": "Fresh owner",
                    "status": "in_progress",
                    "assignee": "ActiveAgent",
                    "updated_at": format_datetime(now - timedelta(hours=1)),
                },
                {
                    "id": "bd-stale",
                    "title": "Old owner",
                    "description": "Surface: `src/old_owner.rs`",
                    "status": "in_progress",
                    "assignee": "OldAgent",
                    "updated_at": format_datetime(now - timedelta(hours=30)),
                },
                {
                    "id": "bd-unassigned",
                    "title": "No owner",
                    "status": "in_progress",
                    "updated_at": format_datetime(now - timedelta(hours=30)),
                },
                {
                    "id": "bd-missing",
                    "title": "Missing updated_at",
                    "status": "in_progress",
                    "assignee": "MissingAgent",
                },
            ],
        )
        report = build_report(repo_root, now=now)
        stale_claims = report["stale_claims"]
        assert_condition(
            stale_claims["policy"] == "report_only_no_auto_reopen_or_reassign",
            "stale claim report must remain report-only",
        )
        assert_condition(
            stale_claim_item(report, "bd-active")["classification"] == "active",
            "recent in-progress work should be active",
        )
        stale_item = stale_claim_item(report, "bd-stale")
        assert_condition(
            stale_item["classification"] == "ambiguous_missing_coordination",
            "old assigned work without mail evidence should be ambiguous",
        )
        assert_condition(
            "message OldAgent" in stale_item["recommended_operator_action"],
            "ambiguous claim should include exact owner follow-up action",
        )
        assert_condition(
            stale_item["operator_bucket"] == "ambiguous",
            "missing Agent Mail/activity evidence should be degraded, not abandonment proof",
        )
        assert_condition(
            "agent_mail_semantic_corruption" in operator_categories(report),
            "ambiguous missing coordination evidence should explain Agent Mail degradation",
        )
        unassigned_item = stale_claim_item(report, "bd-unassigned")
        assert_condition(
            unassigned_item["classification"] == "stale_candidate_unassigned",
            "old unassigned in-progress work should be a stale candidate",
        )
        assert_condition(
            "br update bd-unassigned --status open"
            in unassigned_item["recommended_operator_action"],
            "unassigned stale claim should include exact reopen command",
        )
        missing_item = stale_claim_item(report, "bd-missing")
        assert_condition(
            missing_item["classification"] == "missing_evidence",
            "missing updated_at should be missing evidence",
        )
        assert_condition(
            "do not reopen or reassign bd-missing"
            in missing_item["recommended_operator_action"],
            "missing evidence action should fail closed",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-dirty",
                    "title": "Old owner with dirty surface",
                    "description": "Surface: `src/agent.rs`",
                    "status": "in_progress",
                    "assignee": "DirtyAgent",
                    "updated_at": format_datetime(now - timedelta(hours=30)),
                },
            ],
        )
        report = build_report(
            repo_root,
            now=now,
            stale_claim_dirty_worktree_paths=("src/agent.rs", "README.md"),
            stale_claim_recent_commit_activity={},
        )
        dirty_item = stale_claim_item(report, "bd-dirty")
        assert_condition(
            dirty_item["classification"] == "active_dirty_worktree",
            "dirty files touching a bead surface should keep old work fresh",
        )
        assert_condition(
            dirty_item["dirty_worktree_paths"] == ["src/agent.rs"],
            "dirty-surface evidence should only include matched paths",
        )
        assert_condition(
            "dirty_worktree_conflict" in operator_categories(report),
            "dirty worktree conflicts should have operator explanations",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-commit",
                    "title": "Old owner with recent commit",
                    "status": "in_progress",
                    "assignee": "CommitAgent",
                    "updated_at": format_datetime(now - timedelta(hours=30)),
                },
            ],
        )
        report = build_report(
            repo_root,
            now=now,
            stale_claim_dirty_worktree_paths=(),
            stale_claim_recent_commit_activity={
                "bd-commit": ClaimActivityEvidence(
                    bead_id="bd-commit",
                    timestamp=now - timedelta(hours=1),
                    source="git log:abc123",
                    agent_name=None,
                )
            },
        )
        commit_item = stale_claim_item(report, "bd-commit")
        assert_condition(
            commit_item["classification"] == "active_recent_commit",
            "recent commits mentioning a bead should keep old work fresh",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-single",
                    "title": "Single owner report",
                    "description": "Surface: `src/single_owner.rs`",
                    "status": "in_progress",
                    "assignee": "SoloAgent",
                    "updated_at": format_datetime(now - timedelta(hours=1)),
                },
            ],
        )
        report = build_report(
            repo_root,
            now=now,
            stale_claim_dirty_worktree_paths=(),
            redundant_path_activity={},
        )
        redundant = report["redundant_agent_work"]
        assert_condition(
            redundant["status"] == "ready",
            "clean single-owner work should not produce redundant-work groups",
        )
        assert_condition(
            redundant["summary"]["risk_group_count"] == 0,
            "clean single-owner report should have zero redundant-work risk groups",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-left",
                    "title": "Add shared subsystem guard",
                    "description": "Surface: `src/shared_subsystem.rs`",
                    "status": "in_progress",
                    "assignee": "LeftAgent",
                    "updated_at": format_datetime(now - timedelta(hours=1)),
                },
                {
                    "id": "bd-right",
                    "title": "Harden shared subsystem guard",
                    "description": "Surface: `src/shared_subsystem.rs`",
                    "status": "in_progress",
                    "assignee": "RightAgent",
                    "updated_at": format_datetime(now - timedelta(hours=2)),
                },
            ],
        )
        report = build_report(
            repo_root,
            now=now,
            stale_claim_dirty_worktree_paths=(),
            redundant_path_activity={
                "src/shared_subsystem.rs": ["abc1234 bd-left shared subsystem guard"],
            },
            redundant_agent_mail_health={"status": "ok", "health_level": "green"},
            redundant_agent_mail_health_source="fixture:agent_mail_green",
        )
        groups = report["redundant_agent_work"]["groups"]
        assert_condition(
            any(group["risk"] == "active_surface_collision" for group in groups),
            "two active owners on the same surface should be a duplicate-work risk",
        )
        collision = next(group for group in groups if group["risk"] == "active_surface_collision")
        assert_condition(
            collision["confidence"] >= 0.8,
            "explicit same-surface collision should be high confidence",
        )
        assert_condition(
            all(member["owner_state"] == "active" for member in collision["members"]),
            "fresh in-progress owners should be classified as active",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-stale-owner",
                    "title": "Old owner same lane",
                    "description": "Surface: `scripts/same_lane.py`",
                    "status": "in_progress",
                    "assignee": "StaleAgent",
                    "updated_at": format_datetime(now - timedelta(hours=30)),
                },
                {
                    "id": "bd-new-open",
                    "title": "New request same lane",
                    "description": "Surface: `scripts/same_lane.py`",
                    "status": "open",
                    "updated_at": format_datetime(now - timedelta(hours=1)),
                },
            ],
        )
        report = build_report(
            repo_root,
            now=now,
            stale_claim_dirty_worktree_paths=(),
            redundant_path_activity={},
            redundant_agent_mail_health={
                "status": "error",
                "health_level": "red",
                "semantic_readiness": {
                    "status": "fail",
                    "detail": (
                        "sqlite schema missing required health_check tables: "
                        "projects, agents, messages, message_recipients"
                    ),
                },
                "recovery": {
                    "mode": "corrupt",
                    "next_action": "Run `am doctor repair --yes` or restore from archive backup",
                },
            },
            redundant_agent_mail_health_source="fixture:agent_mail_corrupt",
        )
        redundant = report["redundant_agent_work"]
        assert_condition(
            redundant["coordination"]["classification"] == "degraded_agent_mail",
            "Agent Mail corruption should be explicit degraded coordination, not a hard error",
        )
        stale_groups = [
            group for group in redundant["groups"] if group["risk"] == "stale_owner_overlap"
        ]
        assert_condition(
            stale_groups,
            "stale in-progress work overlapping open work should produce stale_owner_overlap",
        )
        assert_condition(
            redundant["coordination"]["semantic_readiness_detail"]
            == "sqlite schema missing required health_check tables: projects, agents, messages, message_recipients",
            "Agent Mail corruption should preserve missing-table detail",
        )
        assert_condition(
            redundant["coordination"]["recovery_action"]
            == "Run `am doctor repair --yes` or restore from archive backup",
            "Agent Mail corruption should preserve exact recovery action",
        )
        agent_mail_explanations = [
            item
            for item in report["operator_explanations"]["items"]
            if item["category"] == "agent_mail_semantic_corruption"
            and item["affected"] == "agent_mail"
        ]
        assert_condition(
            agent_mail_explanations
            and "projects, agents, messages, message_recipients" in agent_mail_explanations[0]["detail"],
            "operator explanations should include Agent Mail schema corruption detail",
        )
        stale_action = stale_groups[0]["recommended_operator_action"]
        assert_condition(
            "Beads comments/status as the soft lock" in stale_action,
            "degraded Agent Mail guidance should preserve Beads soft-lock fallback",
        )
        assert_condition(
            "Do not reassign, revert, or delete automatically" in stale_action,
            "redundant-work guidance should not tell agents to mutate ownership or discard work",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-repeat",
                    "title": "Add repeated evidence guard",
                    "description": "Surface: `src/repeated_guard.rs`",
                    "status": "open",
                    "updated_at": format_datetime(now - timedelta(hours=1)),
                },
                {
                    "id": "bd-done",
                    "title": "Add repeated evidence guard",
                    "description": "Surface: `src/repeated_guard.rs`",
                    "status": "closed",
                    "closed_at": format_datetime(now - timedelta(hours=2)),
                    "updated_at": format_datetime(now - timedelta(hours=2)),
                    "close_reason": "Implemented in fixture",
                },
            ],
        )
        report = build_report(
            repo_root,
            now=now,
            stale_claim_dirty_worktree_paths=(),
            redundant_path_activity={},
        )
        assert_condition(
            any(
                group["risk"] == "already_closed_duplicate_request"
                for group in report["redundant_agent_work"]["groups"]
            ),
            "open work overlapping a recently closed bead should be flagged as a possible duplicate",
        )

        rch_explanations = build_operator_explanations(
            blocking_issues=[
                {
                    "path": "validation/rch",
                    "kind": "status_not_ready",
                    "detail": "RCH worker workspace shadow prevented authoritative cargo proof",
                    "remediation": "Fix the RCH workspace mapping and rerun the validation command.",
                }
            ],
            stale_claims={"items": []},
        )
        assert_condition(
            rch_explanations["summary"]["category_counts"].get("rch_failure") == 1,
            "RCH workspace-shadow blockers should be classified separately",
        )
        assert_condition(
            rch_explanations["items"][0]["claim_integrity"] == "not_green_until_resolved",
            "RCH blocker explanations must preserve fail-closed claim integrity",
        )

        repo_root = fixture_root()
        make_complete_fixture(repo_root, now)
        write_beads_ledger(
            repo_root,
            [
                {
                    "id": "bd-coordinated",
                    "title": "Old bead with fresh mail",
                    "status": "in_progress",
                    "assignee": "MailAgent",
                    "updated_at": format_datetime(now - timedelta(hours=30)),
                },
            ],
        )
        activity_path = "tests/full_suite_gate/swarm_activity_events.jsonl"
        write_activity_jsonl(
            repo_root,
            activity_path,
            [
                {
                    "schema": "pi.swarm.activity_ledger.v1",
                    "kind": "agent_mail",
                    "timestamp_ms": int((now - timedelta(hours=2)).timestamp() * 1000),
                    "ids": {
                        "bead_id": "bd-coordinated",
                        "agent_name": "MailAgent",
                    },
                    "summary": "owner posted fresh status",
                },
            ],
        )
        report = build_report(
            repo_root,
            now=now,
            stale_claim_activity_fresh_hours=6,
            stale_claim_activity_paths=(activity_path,),
        )
        coordinated_item = stale_claim_item(report, "bd-coordinated")
        assert_condition(
            coordinated_item["classification"] == "active_recent_coordination",
            "fresh optional activity should keep an old bead active",
        )
        assert_condition(
            coordinated_item["evidence_source"].startswith(f"{activity_path}:1:agent_mail"),
            "activity-backed classification should name the evidence source",
        )

        report = build_report(repo_root, now=now, stale_claim_after_hours=48)
        assert_condition(
            stale_claim_item(report, "bd-coordinated")["classification"] == "active",
            "configured stale threshold should control bead-age classification",
        )
    except AssertionError as exc:
        print(f"SELF-TEST FAIL: {exc}")
        return 2

    print("SELF-TEST PASS")
    return 0


def fixture_root() -> Path:
    global _FIXTURE_GIT_TEMPLATE
    if _FIXTURE_GIT_TEMPLATE is None:
        template = Path(tempfile.mkdtemp(prefix="pi_claim_readiness_git_"))
        commands = (
            ("init", "-b", "main"),
            ("config", "user.name", "Pi claim-readiness fixture"),
            ("config", "user.email", "pi-fixture@example.invalid"),
            ("commit", "--allow-empty", "-m", "fixture source baseline"),
        )
        for args in commands:
            result = subprocess.run(
                ["git", "-C", str(template), *args],
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
            if result.returncode != 0:
                raise AssertionError(
                    f"fixture template git {' '.join(args)} failed: {result.stderr.strip()}"
                )
        _FIXTURE_GIT_TEMPLATE = template

    root = Path(tempfile.mkdtemp(prefix="pi_claim_readiness_"))
    clone = subprocess.run(
        ["git", "clone", "--quiet", "--shared", str(_FIXTURE_GIT_TEMPLATE), str(root)],
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if clone.returncode != 0:
        raise AssertionError(f"fixture git clone failed: {clone.stderr.strip()}")
    return root


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="repository root to inspect",
    )
    parser.add_argument(
        "--max-age-days",
        type=int,
        default=DEFAULT_MAX_AGE_DAYS,
        help="freshness threshold for generated release-facing artifacts",
    )
    parser.add_argument(
        "--stale-claim-after-hours",
        type=int,
        default=DEFAULT_STALE_CLAIM_AFTER_HOURS,
        help="age threshold for reporting in-progress beads as stale",
    )
    parser.add_argument(
        "--stale-claim-activity-fresh-hours",
        type=int,
        default=DEFAULT_STALE_CLAIM_ACTIVITY_FRESH_HOURS,
        help="freshness window for optional Agent Mail or activity-ledger evidence",
    )
    parser.add_argument(
        "--stale-claim-activity-jsonl",
        action="append",
        default=None,
        help="optional repo-relative JSONL activity source with bead IDs and timestamps",
    )
    parser.add_argument(
        "--agent-mail-health-json",
        type=Path,
        default=None,
        help="optional Agent Mail health_check JSON payload for redundant-work coordination state",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit machine-readable JSON report",
    )
    parser.add_argument(
        "--gate",
        action="store_true",
        help="exit 1 when release-facing claim evidence is not ready",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="run fixture-backed reporter tests",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        return run_self_test()
    if args.stale_claim_after_hours < 0:
        print("ERROR: --stale-claim-after-hours must be non-negative", file=sys.stderr)
        return 2
    if args.stale_claim_activity_fresh_hours < 0:
        print("ERROR: --stale-claim-activity-fresh-hours must be non-negative", file=sys.stderr)
        return 2

    repo_root = args.repo_root.resolve()
    activity_paths = (
        tuple(args.stale_claim_activity_jsonl)
        if args.stale_claim_activity_jsonl is not None
        else DEFAULT_STALE_CLAIM_ACTIVITY_PATHS
    )
    agent_mail_health = None
    agent_mail_health_source = "not_provided"
    if args.agent_mail_health_json is not None:
        agent_mail_health_source = str(args.agent_mail_health_json)
        agent_mail_health, health_error = load_json(args.agent_mail_health_json)
        if health_error is not None:
            agent_mail_health = {
                "status": "unreadable",
                "health_level": "unknown",
                "error": health_error,
            }
    report = build_report(
        repo_root,
        max_age_days=args.max_age_days,
        stale_claim_after_hours=args.stale_claim_after_hours,
        stale_claim_activity_fresh_hours=args.stale_claim_activity_fresh_hours,
        stale_claim_activity_paths=activity_paths,
        redundant_agent_mail_health=agent_mail_health,
        redundant_agent_mail_health_source=agent_mail_health_source,
    )
    if args.json:
        json.dump(report, sys.stdout, indent=2, sort_keys=True)
        sys.stdout.write("\n")
    else:
        print_text_report(report)

    if args.gate and report["blocking_issue_count"] > 0:
        return 1
    return 0


if __name__ == "__main__":
    with contextlib.suppress(BrokenPipeError):
        sys.exit(main())
