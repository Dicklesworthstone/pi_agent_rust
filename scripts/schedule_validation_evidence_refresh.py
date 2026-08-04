#!/usr/bin/env python3
"""Schedule validation evidence refreshes without running validation.

This planner consumes already-captured signals: stale-evidence queue output,
cargo headroom admission JSON, recent validation results, Beads state, and git
status. It emits operator guidance only. It never runs cargo, mutates Beads,
edits artifacts, calls the network, or regenerates evidence.
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


SCHEDULER_SCHEMA = "pi.validation.refresh_scheduler.v1"
INPUT_SCHEMA = "pi.validation.refresh_scheduler_input.v1"
SELF_TEST_SCHEMA = "pi.validation.refresh_scheduler_self_test.v1"
STALE_QUEUE_SCHEMA = "pi.swarm.stale_evidence_renewal_queue.v1"
INVENTORY_SCHEMA = "pi.traceability.high_value_suite_artifact_inventory.v1"
DEFAULT_RECENT_HOURS = 48
DEFAULT_MAX_ITEMS = 50

CLASSIFICATIONS = (
    "must-refresh",
    "high-value",
    "duplicate-covered",
    "blocked-by-headroom",
    "blocked-by-dirty-worktree",
    "optional",
)
CLASSIFICATION_RANK = {
    "blocked-by-dirty-worktree": 1000,
    "blocked-by-headroom": 950,
    "must-refresh": 900,
    "high-value": 700,
    "duplicate-covered": 500,
    "optional": 100,
}
VALUE_RANK = {"critical": 4, "high": 3, "medium": 2, "low": 1, "info": 0}
COST_RANK = {"very_heavy": 4, "heavy": 3, "medium": 2, "light": 1, "unknown": 0}
STALE_REASON_CODES = {
    "expired",
    "missing_source_ref",
    "contract_schema_changed",
    "contract_newer_than_artifact",
    "blocked_rch",
    "malformed_json",
    "missing_generated_at",
    "stale",
    "missing",
    "invalid_json",
    "schema_mismatch",
    "status_not_ready",
    "no_data",
    "provenance_mismatch",
}
FRESH_REASON_CODES = {"fresh", "ready", "current", "historical_snapshot"}
PASS_STATUSES = {"pass", "passed", "ok", "success", "ready"}
ACTIVE_BEAD_STATUSES = {"open", "in_progress"}
HEAVY_COMMAND_MARKERS = (
    "cargo test",
    "cargo check",
    "cargo clippy",
    "cargo build",
    "rch exec",
    "scripts/cargo_headroom.sh",
)


class SchedulerError(Exception):
    """Raised when scheduler inputs are malformed or unusable."""


def parse_utc(raw: object) -> datetime | None:
    if not isinstance(raw, str):
        return None
    text = raw.strip()
    if not text:
        return None
    if text.endswith("Z"):
        text = f"{text[:-1]}+00:00"
    try:
        value = datetime.fromisoformat(text)
    except ValueError:
        return None
    if value.tzinfo is None:
        value = value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def iso_datetime(value: datetime | None) -> str | None:
    if value is None:
        return None
    return value.astimezone(timezone.utc).replace(microsecond=0).isoformat()


def json_dumps(value: Any) -> str:
    return json.dumps(value, indent=2, sort_keys=True) + "\n"


def load_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise SchedulerError(f"missing JSON file: {path}") from exc
    except json.JSONDecodeError as exc:
        raise SchedulerError(f"malformed JSON file {path}: {exc}") from exc


def no_overwrite_write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        raise SchedulerError(f"refusing to overwrite existing output: {path}")
    path.write_text(text, encoding="utf-8")


def as_list(value: Any) -> list[Any]:
    if value is None:
        return []
    if isinstance(value, list):
        return value
    return [value]


def string_list(value: Any) -> list[str]:
    result: list[str] = []
    for item in as_list(value):
        if isinstance(item, str) and item.strip():
            result.append(item.strip())
    return sorted(set(result))


def bool_value(value: Any, *, default: bool = False) -> bool:
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        lowered = value.strip().lower()
        if lowered in {"1", "true", "yes", "y"}:
            return True
        if lowered in {"0", "false", "no", "n"}:
            return False
    return default


def normalized_value(raw: Any, *, default: str = "medium") -> str:
    if isinstance(raw, str):
        lowered = raw.strip().lower().replace("-", "_")
        if lowered in VALUE_RANK:
            return lowered
        if lowered == "critical_high":
            return "critical"
    return default


def first_string(*values: Any) -> str | None:
    for value in values:
        if isinstance(value, str) and value.strip():
            return value.strip()
    return None


def infer_cost_class(command_text: str | None, fallback: Any = None) -> str:
    if isinstance(fallback, str):
        lowered = fallback.strip().lower().replace("-", "_")
        if lowered in COST_RANK:
            return lowered
    text = (command_text or "").lower()
    if "--all-targets" in text or "run_all.sh" in text or "ext_quality_pipeline" in text:
        return "very_heavy"
    if any(marker in text for marker in HEAVY_COMMAND_MARKERS):
        return "heavy"
    if "python3 -m json.tool" in text or "git status" in text or "br " in text:
        return "light"
    if "python3 " in text or "scripts/" in text:
        return "medium"
    return "unknown"


def infer_safety_class(command_text: str | None, fallback: Any = None) -> str:
    if isinstance(fallback, str) and fallback.strip():
        return fallback.strip()
    text = (command_text or "").lower()
    if not text:
        return "manual_review"
    if "rch exec" in text or "cargo " in text or "scripts/cargo_headroom.sh" in text:
        return "rch_validation"
    if "--self-test" in text:
        return "python_self_test"
    if "python3 -m json.tool" in text or "git status" in text or text.startswith("br "):
        return "read_only_probe"
    if "python3 " in text or "scripts/" in text:
        return "local_light_validation"
    return "manual_review"


def source_pointer(path: Path | None, suffix: str) -> str:
    if path is None:
        return suffix
    return f"{path}#{suffix}" if suffix else str(path)


def command_from_queue_item(item: dict[str, Any]) -> tuple[str | None, str | None]:
    commands = item.get("renewal_commands")
    if not isinstance(commands, list):
        return None, None
    for command in commands:
        if not isinstance(command, dict):
            continue
        text = first_string(command.get("command"))
        if text is not None:
            return text, first_string(command.get("safety_class"))
    return None, None


def candidate_from_stale_queue_item(item: dict[str, Any], *, source_path: Path | None) -> dict[str, Any]:
    command_text, safety_class = command_from_queue_item(item)
    artifact_path = first_string(item.get("artifact_path"), item.get("source_artifact"))
    reason_codes = string_list(item.get("reason_codes"))
    severity = normalized_value(item.get("severity"), default="medium")
    return {
        "id": first_string(item.get("id"), artifact_path, "stale-evidence") or "stale-evidence",
        "source_artifact": artifact_path or source_pointer(source_path, "queue"),
        "freshness_reasons": reason_codes or [first_string(item.get("status"), "stale") or "stale"],
        "required": bool_value(item.get("blocks_dropin_claim")) or severity == "critical",
        "value": severity,
        "estimated_cost_class": infer_cost_class(command_text, item.get("estimated_cost_class")),
        "selected_command": command_text,
        "safety_class": infer_safety_class(command_text, safety_class),
        "changed_surfaces": string_list(item.get("missing_source_refs")) + string_list(artifact_path),
        "dedupe_key": first_string(item.get("dedupe_key"), artifact_path, item.get("id")),
        "requires_headroom": infer_cost_class(command_text) in {"heavy", "very_heavy"},
        "requires_clean_worktree": bool_value(item.get("requires_clean_worktree"), default=severity == "critical"),
        "strict_evidence_refresh": bool_value(item.get("blocks_dropin_claim")),
        "source_kind": "stale_evidence_queue",
        "source_pointer": source_pointer(source_path, f"queue.{item.get('id', '')}"),
    }


def candidates_from_stale_queue(payload: Any, *, source_path: Path | None) -> list[dict[str, Any]]:
    if not isinstance(payload, dict):
        raise SchedulerError("stale queue JSON must be an object")
    if payload.get("schema") not in {None, STALE_QUEUE_SCHEMA}:
        raise SchedulerError(f"stale queue schema mismatch: {payload.get('schema')}")
    queue = payload.get("queue")
    if not isinstance(queue, list):
        return []
    return [
        candidate_from_stale_queue_item(item, source_path=source_path)
        for item in queue
        if isinstance(item, dict)
    ]


def candidates_from_inventory(payload: Any, *, source_path: Path | None) -> list[dict[str, Any]]:
    if not isinstance(payload, dict):
        raise SchedulerError("high-value inventory JSON must be an object")
    if payload.get("schema") not in {None, INVENTORY_SCHEMA}:
        raise SchedulerError(f"high-value inventory schema mismatch: {payload.get('schema')}")
    candidates: list[dict[str, Any]] = []
    suites = payload.get("selected_suites")
    if not isinstance(suites, list):
        return candidates
    for suite in suites:
        if not isinstance(suite, dict):
            continue
        suite_id = first_string(suite.get("id"), suite.get("coverage_area"))
        if suite_id is None:
            continue
        command_text = first_string(suite.get("deterministic_replay_command"))
        coverage_area = first_string(suite.get("coverage_area"), suite_id) or suite_id
        candidates.append(
            {
                "id": f"high-value-{suite_id}",
                "source_artifact": source_pointer(source_path, f"selected_suites.{suite_id}"),
                "freshness_reasons": ["high_value_inventory"],
                "required": False,
                "value": "high",
                "estimated_cost_class": infer_cost_class(command_text),
                "selected_command": command_text,
                "safety_class": infer_safety_class(command_text),
                "changed_surfaces": string_list(suite.get("test_paths")),
                "dedupe_key": coverage_area,
                "requires_headroom": infer_cost_class(command_text) in {"heavy", "very_heavy"},
                "requires_clean_worktree": False,
                "source_kind": "high_value_inventory",
                "source_pointer": source_pointer(source_path, f"selected_suites.{suite_id}"),
            }
        )
    return candidates


def normalize_candidate(raw: dict[str, Any], index: int) -> dict[str, Any]:
    command_text = first_string(
        raw.get("selected_command"),
        raw.get("command"),
        raw.get("deterministic_replay_command"),
    )
    source_artifact = first_string(
        raw.get("source_artifact"),
        raw.get("artifact_path"),
        raw.get("path"),
        f"candidate[{index}]",
    )
    candidate_id = first_string(raw.get("id"), source_artifact, f"candidate-{index}") or f"candidate-{index}"
    cost_class = infer_cost_class(command_text, raw.get("estimated_cost_class"))
    freshness_reasons = string_list(
        raw.get("freshness_reasons")
        or raw.get("freshness_reason")
        or raw.get("reason_codes")
        or raw.get("status")
    )
    if not freshness_reasons:
        freshness_reasons = ["unspecified"]
    changed_surfaces = sorted(
        set(
            string_list(raw.get("changed_surfaces"))
            + string_list(raw.get("test_paths"))
            + string_list(raw.get("source_paths"))
            + string_list(raw.get("paths"))
        )
    )
    return {
        "id": candidate_id,
        "source_artifact": source_artifact,
        "freshness_reasons": freshness_reasons,
        "required": bool_value(raw.get("required")) or bool_value(raw.get("release_blocking")),
        "value": normalized_value(raw.get("value") or raw.get("severity"), default="medium"),
        "estimated_cost_class": cost_class,
        "selected_command": command_text,
        "safety_class": infer_safety_class(command_text, raw.get("safety_class")),
        "changed_surfaces": changed_surfaces,
        "dedupe_key": first_string(raw.get("dedupe_key"), raw.get("coverage_area"), candidate_id),
        "requires_headroom": bool_value(
            raw.get("requires_headroom"),
            default=cost_class in {"heavy", "very_heavy"},
        ),
        "requires_clean_worktree": bool_value(
            raw.get("requires_clean_worktree"),
            default=bool_value(raw.get("required")) and cost_class in {"heavy", "very_heavy"},
        ),
        "strict_evidence_refresh": bool_value(raw.get("strict_evidence_refresh")),
        "allowed_dirty_files": string_list(raw.get("allowed_dirty_files")),
        "bead_id": first_string(raw.get("bead_id"), raw.get("issue_id")),
        "source_kind": first_string(raw.get("source_kind"), "candidate_input"),
        "source_pointer": first_string(raw.get("source_pointer"), source_artifact),
    }


def candidate_is_stale(candidate: dict[str, Any]) -> bool:
    reasons = {str(reason).lower() for reason in candidate.get("freshness_reasons", [])}
    if reasons & STALE_REASON_CODES:
        return True
    if reasons and reasons <= FRESH_REASON_CODES:
        return False
    return bool_value(candidate.get("stale"), default=False)


def normalize_recent_validations(value: Any) -> list[dict[str, Any]]:
    if isinstance(value, dict):
        candidates = (
            value.get("recent_validations")
            or value.get("validations")
            or value.get("results")
            or value.get("validation_results")
        )
    else:
        candidates = value
    normalized: list[dict[str, Any]] = []
    for index, raw in enumerate(as_list(candidates)):
        if not isinstance(raw, dict):
            continue
        command_text = first_string(raw.get("command"), raw.get("selected_command"))
        covered_surfaces = sorted(
            set(
                string_list(raw.get("covered_surfaces"))
                + string_list(raw.get("changed_surfaces"))
                + string_list(raw.get("test_paths"))
                + string_list(raw.get("source_paths"))
            )
        )
        normalized.append(
            {
                "id": first_string(raw.get("id"), raw.get("name"), f"validation-{index}"),
                "status": first_string(raw.get("status"), raw.get("verdict"), "unknown") or "unknown",
                "generated_at": parse_utc(raw.get("generated_at") or raw.get("observed_at")),
                "command": command_text,
                "estimated_cost_class": infer_cost_class(command_text, raw.get("estimated_cost_class")),
                "covered_surfaces": covered_surfaces,
                "dedupe_key": first_string(raw.get("dedupe_key"), raw.get("coverage_area")),
                "evidence_artifact": first_string(raw.get("evidence_artifact"), raw.get("artifact_path")),
            }
        )
    return normalized


def parse_git_status_text(text: str) -> dict[str, Any]:
    changed_files: list[str] = []
    raw_count = 0
    for line in text.splitlines():
        if not line.strip() or line.startswith("##"):
            continue
        raw_count += 1
        path = line[3:] if len(line) > 3 else line.strip()
        if " -> " in path:
            path = path.split(" -> ", 1)[1]
        if path:
            changed_files.append(path.strip())
    return {
        "dirty": bool(changed_files),
        "changed_files": sorted(set(changed_files)),
        "raw_status_count": raw_count,
    }


def normalize_git(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict):
        return {"dirty": False, "changed_files": [], "raw_status_count": 0}
    changed_files = string_list(value.get("changed_files") or value.get("dirty_files"))
    dirty = bool_value(value.get("dirty"), default=bool(changed_files))
    return {
        "dirty": dirty,
        "changed_files": changed_files,
        "raw_status_count": int(value.get("raw_status_count") or len(changed_files)),
        "head": first_string(value.get("head"), value.get("git_commit")),
    }


def normalize_beads(value: Any) -> dict[str, Any]:
    if isinstance(value, dict):
        issues = value.get("issues") or value.get("beads") or value.get("items")
    else:
        issues = value
    normalized: list[dict[str, Any]] = []
    for raw in as_list(issues):
        if not isinstance(raw, dict):
            continue
        status = first_string(raw.get("status"), "unknown") or "unknown"
        normalized.append(
            {
                "id": first_string(raw.get("id"), raw.get("issue_id"), "unknown") or "unknown",
                "title": first_string(raw.get("title"), ""),
                "status": status,
                "assignee": first_string(raw.get("assignee"), raw.get("owner")),
                "priority": raw.get("priority"),
                "labels": string_list(raw.get("labels")),
            }
        )
    active = [issue for issue in normalized if issue["status"] in ACTIVE_BEAD_STATUSES]
    in_progress = [issue for issue in normalized if issue["status"] == "in_progress"]
    return {
        "issues": normalized,
        "active_count": len(active),
        "in_progress_count": len(in_progress),
    }


def headroom_signal(pressure: dict[str, Any]) -> dict[str, Any]:
    admission = pressure.get("cargo_admission")
    if not isinstance(admission, dict):
        return {
            "status": "unknown",
            "blocked": True,
            "reasons": ["cargo_admission_missing"],
            "decision": None,
            "admission_action": None,
        }
    decision = first_string(admission.get("decision"), "unknown") or "unknown"
    admission_action = first_string(admission.get("admission_action"), "unknown") or "unknown"
    reason = first_string(admission.get("reason"))
    reasons = [reason] if reason else []
    blocked = decision in {"backoff", "deny", "blocked"} or admission_action == "defer"
    local = admission.get("local_process_pressure")
    if isinstance(local, dict) and first_string(local.get("recommended_action")) == "defer":
        blocked = True
        reasons.append("local_process_pressure")
    forecast = admission.get("rch_queue_forecast")
    if isinstance(forecast, dict) and first_string(forecast.get("recommended_action")) == "backoff":
        blocked = True
        forecast_reason = first_string(forecast.get("reason"))
        reasons.append(f"rch_{forecast_reason or 'backoff'}")
    return {
        "status": "blocked" if blocked else "available",
        "blocked": blocked,
        "reasons": sorted(set(reasons)),
        "decision": decision,
        "admission_action": admission_action,
        "command_class": first_string(admission.get("command_class")),
    }


def git_signal(candidate: dict[str, Any], git: dict[str, Any]) -> dict[str, Any]:
    changed_files = string_list(git.get("changed_files"))
    allowed = set(string_list(candidate.get("allowed_dirty_files")))
    if allowed:
        blocking_files = [path for path in changed_files if path not in allowed]
    else:
        blocking_files = changed_files
    blocked = bool_value(git.get("dirty")) and bool_value(candidate.get("requires_clean_worktree")) and bool(blocking_files)
    return {
        "status": "dirty" if bool_value(git.get("dirty")) else "clean",
        "blocked": blocked,
        "dirty_files": changed_files,
        "blocking_dirty_files": blocking_files,
    }


def beads_signal(candidate: dict[str, Any], beads: dict[str, Any], current_agent: str | None) -> dict[str, Any]:
    candidate_id = str(candidate.get("id") or "")
    bead_id = str(candidate.get("bead_id") or "")
    matches: list[dict[str, Any]] = []
    for issue in beads.get("issues", []):
        issue_id = str(issue.get("id") or "")
        title = str(issue.get("title") or "")
        if bead_id and issue_id == bead_id:
            matches.append(issue)
        elif candidate_id and (candidate_id == issue_id or candidate_id in title):
            matches.append(issue)
    contended = [
        issue
        for issue in matches
        if issue.get("status") == "in_progress"
        and issue.get("assignee")
        and current_agent
        and issue.get("assignee") != current_agent
    ]
    return {
        "active_count": beads.get("active_count", 0),
        "in_progress_count": beads.get("in_progress_count", 0),
        "matching_issue_ids": [issue.get("id") for issue in matches],
        "contended": bool(contended),
    }


def validation_pass_is_recent(validation: dict[str, Any], *, generated_at: datetime, recent_hours: int) -> bool:
    status = str(validation.get("status") or "").lower()
    if status not in PASS_STATUSES:
        return False
    timestamp = validation.get("generated_at")
    if not isinstance(timestamp, datetime):
        return True
    return generated_at - timestamp <= timedelta(hours=recent_hours)


def find_duplicate_coverage(
    candidate: dict[str, Any],
    validations: list[dict[str, Any]],
    *,
    generated_at: datetime,
    recent_hours: int,
) -> dict[str, Any] | None:
    candidate_key = first_string(candidate.get("dedupe_key"))
    candidate_surfaces = set(string_list(candidate.get("changed_surfaces")))
    candidate_cost = COST_RANK.get(str(candidate.get("estimated_cost_class")), 0)
    for validation in validations:
        if not validation_pass_is_recent(validation, generated_at=generated_at, recent_hours=recent_hours):
            continue
        validation_cost = COST_RANK.get(str(validation.get("estimated_cost_class")), 0)
        if validation_cost > candidate_cost:
            continue
        validation_key = first_string(validation.get("dedupe_key"))
        validation_surfaces = set(string_list(validation.get("covered_surfaces")))
        key_matches = bool(candidate_key and validation_key and candidate_key == validation_key)
        surface_matches = bool(candidate_surfaces and candidate_surfaces.issubset(validation_surfaces))
        if key_matches or surface_matches:
            return validation
    return None


def candidate_rank(candidate: dict[str, Any], classification: str, verdict: str) -> tuple[int, int, int, str]:
    fail_closed_bonus = 100 if verdict == "fail_closed" else 0
    return (
        CLASSIFICATION_RANK.get(classification, 0) + fail_closed_bonus,
        VALUE_RANK.get(str(candidate.get("value")), 0),
        -COST_RANK.get(str(candidate.get("estimated_cost_class")), 0),
        str(candidate.get("id")),
    )


def evaluate_candidate(
    candidate: dict[str, Any],
    *,
    pressure: dict[str, Any],
    git: dict[str, Any],
    beads: dict[str, Any],
    validations: list[dict[str, Any]],
    generated_at: datetime,
    recent_hours: int,
    current_agent: str | None,
) -> dict[str, Any]:
    stale = candidate_is_stale(candidate)
    required = bool_value(candidate.get("required"))
    strict = bool_value(candidate.get("strict_evidence_refresh"))
    headroom = headroom_signal(pressure)
    git_state = git_signal(candidate, git)
    bead_state = beads_signal(candidate, beads, current_agent)
    duplicate = None if strict else find_duplicate_coverage(
        candidate,
        validations,
        generated_at=generated_at,
        recent_hours=recent_hours,
    )

    reason_codes: list[str] = []
    selected_command = first_string(candidate.get("selected_command"))
    safety_class = first_string(candidate.get("safety_class"), "manual_review") or "manual_review"

    if duplicate is not None:
        classification = "duplicate-covered"
        verdict = "skip_covered"
        reason_codes.append("recent_validation_covers_surface")
        selected_command = first_string(duplicate.get("command"), selected_command)
        safety_class = "reused_validation_evidence"
    elif git_state["blocked"]:
        classification = "blocked-by-dirty-worktree"
        verdict = "fail_closed" if stale and required else "defer"
        reason_codes.append("dirty_worktree_blocks_refresh")
        safety_class = "blocked_no_execution"
    elif bool_value(candidate.get("requires_headroom")) and bool_value(headroom.get("blocked")):
        classification = "blocked-by-headroom"
        verdict = "fail_closed" if stale and required else "defer"
        reason_codes.extend(headroom.get("reasons") or ["headroom_blocks_refresh"])
        safety_class = "blocked_no_execution"
    elif stale and required:
        classification = "must-refresh"
        verdict = "schedule"
        reason_codes.append("required_stale_evidence")
    elif stale or VALUE_RANK.get(str(candidate.get("value")), 0) >= VALUE_RANK["high"]:
        classification = "high-value"
        verdict = "schedule"
        reason_codes.append("high_value_refresh_candidate")
    else:
        classification = "optional"
        verdict = "skip_optional"
        reason_codes.append("fresh_or_low_value")

    assert classification in CLASSIFICATIONS
    freshness_reasons = string_list(candidate.get("freshness_reasons"))
    if stale:
        reason_codes.append("stale_signal_present")
    return {
        "id": candidate["id"],
        "classification": classification,
        "verdict": verdict,
        "source_artifact": candidate["source_artifact"],
        "source_kind": candidate.get("source_kind"),
        "source_pointer": candidate.get("source_pointer"),
        "freshness_reason": ",".join(freshness_reasons),
        "freshness_reasons": freshness_reasons,
        "required": required,
        "value": candidate.get("value"),
        "estimated_cost_class": candidate.get("estimated_cost_class"),
        "pressure_signals": {
            "headroom": headroom,
            "git": git_state,
            "beads": bead_state,
            "recent_validation": {
                "status": "covered" if duplicate is not None else "not_covered",
                "validation_id": duplicate.get("id") if duplicate else None,
                "evidence_artifact": duplicate.get("evidence_artifact") if duplicate else None,
            },
        },
        "selected_command": selected_command,
        "safety_class": safety_class,
        "reason_codes": sorted(set(reason_codes)),
        "changed_surfaces": string_list(candidate.get("changed_surfaces")),
        "dedupe_key": candidate.get("dedupe_key"),
    }


def collect_candidates(payload: dict[str, Any], source_paths: dict[str, Path | None]) -> list[dict[str, Any]]:
    raw_candidates: list[dict[str, Any]] = []
    stale_queue = payload.get("stale_evidence_renewal_queue")
    if stale_queue is not None:
        raw_candidates.extend(
            candidates_from_stale_queue(stale_queue, source_path=source_paths.get("stale_queue"))
        )
    inventory = payload.get("high_value_suite_inventory")
    if inventory is not None:
        raw_candidates.extend(
            candidates_from_inventory(inventory, source_path=source_paths.get("inventory"))
        )
    for raw in as_list(payload.get("candidates")):
        if isinstance(raw, dict):
            raw_candidates.append(raw)
    return [normalize_candidate(candidate, index) for index, candidate in enumerate(raw_candidates)]


def build_plan(
    payload: dict[str, Any],
    *,
    generated_at: datetime,
    recent_hours: int,
    max_items: int,
    source_paths: dict[str, Path | None] | None = None,
) -> dict[str, Any]:
    source_paths = source_paths or {}
    candidates = collect_candidates(payload, source_paths)
    pressure = payload.get("pressure") if isinstance(payload.get("pressure"), dict) else {}
    if isinstance(payload.get("cargo_admission"), dict):
        pressure = {**pressure, "cargo_admission": payload["cargo_admission"]}
    git = normalize_git(payload.get("git"))
    beads = normalize_beads(payload.get("beads"))
    validations = normalize_recent_validations(payload.get("recent_validations"))
    current_agent = first_string(payload.get("current_agent"))

    evaluated = [
        evaluate_candidate(
            candidate,
            pressure=pressure,
            git=git,
            beads=beads,
            validations=validations,
            generated_at=generated_at,
            recent_hours=recent_hours,
            current_agent=current_agent,
        )
        for candidate in candidates
    ]
    evaluated.sort(
        key=lambda item: candidate_rank(
            item,
            str(item.get("classification")),
            str(item.get("verdict")),
        ),
        reverse=True,
    )
    limited = evaluated[:max_items]
    counts = {classification: 0 for classification in CLASSIFICATIONS}
    for item in evaluated:
        counts[str(item["classification"])] += 1
    fail_closed_count = sum(1 for item in evaluated if item["verdict"] == "fail_closed")
    defer_count = sum(1 for item in evaluated if item["verdict"] == "defer")
    schedule_count = sum(1 for item in evaluated if item["verdict"] == "schedule")
    duplicate_count = counts["duplicate-covered"]
    if fail_closed_count:
        verdict = "fail_closed"
    elif defer_count:
        verdict = "defer"
    elif schedule_count:
        verdict = "schedule"
    elif duplicate_count:
        verdict = "covered"
    else:
        verdict = "optional"
    return {
        "schema": SCHEDULER_SCHEMA,
        "generated_at": iso_datetime(generated_at),
        "verdict": verdict,
        "recent_validation_window_hours": recent_hours,
        "summary": {
            "candidate_count": len(evaluated),
            "emitted_candidate_count": len(limited),
            "classification_counts": counts,
            "fail_closed_count": fail_closed_count,
            "defer_count": defer_count,
            "schedule_count": schedule_count,
            "duplicate_covered_count": duplicate_count,
        },
        "pressure_summary": {
            "headroom": headroom_signal(pressure),
            "git_dirty": bool_value(git.get("dirty")),
            "dirty_file_count": len(string_list(git.get("changed_files"))),
            "beads_active_count": beads.get("active_count", 0),
            "beads_in_progress_count": beads.get("in_progress_count", 0),
            "recent_validation_count": len(validations),
        },
        "plan": limited,
        "guardrails": {
            "no_validation_execution": True,
            "no_evidence_regeneration": True,
            "no_beads_mutation": True,
            "no_git_mutation": True,
            "commands_require_operator_execution": True,
            "fail_closed_on_required_stale_unsafe_refresh": True,
        },
    }


def merge_payload_from_args(args: argparse.Namespace) -> tuple[dict[str, Any], dict[str, Path | None]]:
    payload: dict[str, Any] = {"schema": INPUT_SCHEMA}
    source_paths: dict[str, Path | None] = {}
    if args.input_json is not None:
        loaded = load_json(args.input_json)
        if not isinstance(loaded, dict):
            raise SchedulerError("--input-json must contain an object")
        payload.update(loaded)
        source_paths["input"] = args.input_json
    if args.candidate_json is not None:
        loaded = load_json(args.candidate_json)
        if isinstance(loaded, dict):
            payload["candidates"] = as_list(payload.get("candidates")) + as_list(
                loaded.get("candidates") or loaded.get("items") or loaded.get("plan")
            )
        elif isinstance(loaded, list):
            payload["candidates"] = as_list(payload.get("candidates")) + loaded
        else:
            raise SchedulerError("--candidate-json must contain an object or list")
        source_paths["candidates"] = args.candidate_json
    if args.stale_queue_json is not None:
        payload["stale_evidence_renewal_queue"] = load_json(args.stale_queue_json)
        source_paths["stale_queue"] = args.stale_queue_json
    if args.inventory_json is not None:
        payload["high_value_suite_inventory"] = load_json(args.inventory_json)
        source_paths["inventory"] = args.inventory_json
    if args.cargo_admission_json is not None:
        pressure = payload.get("pressure") if isinstance(payload.get("pressure"), dict) else {}
        pressure["cargo_admission"] = load_json(args.cargo_admission_json)
        payload["pressure"] = pressure
        source_paths["cargo_admission"] = args.cargo_admission_json
    if args.validation_results_json is not None:
        payload["recent_validations"] = load_json(args.validation_results_json)
        source_paths["recent_validations"] = args.validation_results_json
    if args.beads_json is not None:
        payload["beads"] = load_json(args.beads_json)
        source_paths["beads"] = args.beads_json
    if args.git_status_file is not None:
        payload["git"] = parse_git_status_text(args.git_status_file.read_text(encoding="utf-8"))
        source_paths["git_status"] = args.git_status_file
    if args.current_agent:
        payload["current_agent"] = args.current_agent
    return payload, source_paths


def allow_admission() -> dict[str, Any]:
    return {
        "schema": "pi.cargo_headroom.admission.v1",
        "decision": "allow",
        "admission_action": "allow",
        "reason": "rch_available",
        "command_class": "heavy",
        "local_process_pressure": {"recommended_action": "run"},
        "rch_queue_forecast": {"recommended_action": "run", "reason": "queue_snapshot_ok"},
    }


def backoff_admission(reason: str) -> dict[str, Any]:
    return {
        "schema": "pi.cargo_headroom.admission.v1",
        "decision": "backoff",
        "admission_action": "defer",
        "reason": reason,
        "command_class": "heavy",
        "local_process_pressure": {"recommended_action": "run"},
        "rch_queue_forecast": {"recommended_action": "backoff", "reason": reason},
    }


def run_self_test() -> int:
    generated_at = datetime(2026, 5, 16, 12, 0, tzinfo=timezone.utc)
    scenarios = [
        {
            "id": "dirty-required-refresh-fails-closed",
            "payload": {
                "pressure": {"cargo_admission": allow_admission()},
                "git": {"dirty": True, "changed_files": ["src/session.rs"]},
                "candidates": [
                    {
                        "id": "dirty-required",
                        "source_artifact": "docs/evidence/session-refresh.json",
                        "freshness_reasons": ["expired"],
                        "required": True,
                        "value": "critical",
                        "selected_command": "rch exec -- cargo test session",
                        "requires_clean_worktree": True,
                    }
                ],
            },
            "expected": {"dirty-required": "blocked-by-dirty-worktree"},
            "verdict": "fail_closed",
        },
        {
            "id": "headroom-required-refresh-fails-closed",
            "payload": {
                "pressure": {"cargo_admission": backoff_admission("insufficient_headroom")},
                "git": {"dirty": False, "changed_files": []},
                "stale_evidence_renewal_queue": {
                    "schema": STALE_QUEUE_SCHEMA,
                    "queue": [
                        {
                            "id": "dropin-verdict",
                            "artifact_path": "docs/evidence/dropin-certification-verdict.json",
                            "status": "renewal_recommended",
                            "severity": "critical",
                            "reason_codes": ["expired"],
                            "blocks_dropin_claim": True,
                            "renewal_commands": [
                                {
                                    "command": "rch exec -- cargo check --all-targets",
                                    "safety_class": "rch_validation",
                                }
                            ],
                        }
                    ],
                },
            },
            "expected": {"dropin-verdict": "blocked-by-headroom"},
            "verdict": "fail_closed",
        },
        {
            "id": "mixed-prioritization",
            "payload": {
                "pressure": {"cargo_admission": allow_admission()},
                "git": {"dirty": False, "changed_files": ["src/session.rs"]},
                "beads": [
                    {
                        "id": "bd-63x3v.8.6",
                        "title": "Schedule validation evidence refresh by value and pressure",
                        "status": "in_progress",
                        "assignee": "AmberOsprey",
                    }
                ],
                "current_agent": "AmberOsprey",
                "recent_validations": [
                    {
                        "id": "session-narrow-proof",
                        "status": "pass",
                        "generated_at": "2026-05-16T11:15:00Z",
                        "command": "rch exec -- cargo test session::tests",
                        "covered_surfaces": ["src/session.rs"],
                        "dedupe_key": "sessions",
                        "evidence_artifact": "tests/e2e_results/session/test-log.jsonl",
                    }
                ],
                "candidates": [
                    {
                        "id": "required-session-artifact",
                        "source_artifact": "docs/evidence/session-artifact.json",
                        "freshness_reasons": ["expired"],
                        "required": True,
                        "value": "critical",
                        "selected_command": "python3 scripts/check_swarm_runpack_freshness.py docs/evidence/session-artifact.json --json",
                        "requires_headroom": False,
                        "requires_clean_worktree": False,
                        "strict_evidence_refresh": True,
                    },
                    {
                        "id": "broad-session-gate",
                        "source_artifact": "docs/evidence/high-value-suite-artifact-inventory.json#sessions",
                        "freshness_reasons": ["high_value_inventory"],
                        "value": "high",
                        "selected_command": "rch exec -- cargo test --all-targets",
                        "changed_surfaces": ["src/session.rs"],
                        "dedupe_key": "sessions",
                    },
                    {
                        "id": "provider-streaming-high-value",
                        "source_artifact": "docs/evidence/high-value-suite-artifact-inventory.json#provider",
                        "freshness_reasons": ["high_value_inventory"],
                        "value": "high",
                        "selected_command": (
                            "PI_PROVIDER_REPLAY_GIT_COMMIT=$(git rev-parse HEAD) "
                            "rch exec -- cargo test --test provider_streaming"
                        ),
                        "changed_surfaces": ["tests/provider_streaming.rs"],
                        "dedupe_key": "provider_streaming",
                    },
                    {
                        "id": "fresh-low-value",
                        "source_artifact": "docs/evidence/example.json",
                        "freshness_reasons": ["fresh"],
                        "value": "low",
                        "selected_command": "python3 -m json.tool docs/evidence/example.json",
                    },
                ],
            },
            "expected": {
                "required-session-artifact": "must-refresh",
                "broad-session-gate": "duplicate-covered",
                "provider-streaming-high-value": "high-value",
                "fresh-low-value": "optional",
            },
            "verdict": "schedule",
        },
    ]
    results = []
    for scenario in scenarios:
        plan = build_plan(
            scenario["payload"],
            generated_at=generated_at,
            recent_hours=DEFAULT_RECENT_HOURS,
            max_items=DEFAULT_MAX_ITEMS,
        )
        assert plan["verdict"] == scenario["verdict"], scenario["id"]
        by_id = {item["id"]: item for item in plan["plan"]}
        for candidate_id, expected_classification in scenario["expected"].items():
            assert by_id[candidate_id]["classification"] == expected_classification, (
                scenario["id"],
                candidate_id,
                by_id[candidate_id]["classification"],
            )
        results.append(
            {
                "id": scenario["id"],
                "verdict": plan["verdict"],
                "classification_counts": plan["summary"]["classification_counts"],
            }
        )
    print(
        json_dumps(
            {
                "schema": SELF_TEST_SCHEMA,
                "status": "pass",
                "scenario_count": len(results),
                "scenarios": results,
            }
        )
    )
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input-json", type=Path, help="combined scheduler input JSON")
    parser.add_argument("--candidate-json", type=Path, help="candidate list or object with candidates")
    parser.add_argument("--stale-queue-json", type=Path, help="stale evidence renewal queue JSON")
    parser.add_argument("--inventory-json", type=Path, help="high-value suite inventory JSON")
    parser.add_argument("--cargo-admission-json", type=Path, help="cargo_headroom --admit-only JSON")
    parser.add_argument("--validation-results-json", type=Path, help="recent validation result JSON")
    parser.add_argument("--beads-json", type=Path, help="br list/show JSON")
    parser.add_argument("--git-status-file", type=Path, help="git status --porcelain output")
    parser.add_argument("--current-agent", help="agent name for Beads contention projection")
    parser.add_argument("--generated-at", help="override generated timestamp")
    parser.add_argument("--recent-hours", type=int, default=DEFAULT_RECENT_HOURS)
    parser.add_argument("--max-items", type=int, default=DEFAULT_MAX_ITEMS)
    parser.add_argument("--out-json", type=Path, help="write scheduler JSON; refuses to overwrite")
    parser.add_argument("--json", action="store_true", help="print scheduler JSON")
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.self_test:
            return run_self_test()
        if args.recent_hours < 0:
            raise SchedulerError("--recent-hours must be non-negative")
        if args.max_items < 0:
            raise SchedulerError("--max-items must be non-negative")
        payload, source_paths = merge_payload_from_args(args)
        generated_at = parse_utc(args.generated_at) or datetime.now(timezone.utc)
        plan = build_plan(
            payload,
            generated_at=generated_at,
            recent_hours=args.recent_hours,
            max_items=args.max_items,
            source_paths=source_paths,
        )
        if args.out_json:
            no_overwrite_write(args.out_json, json_dumps(plan))
        if args.json or not args.out_json:
            print(json_dumps(plan))
        return 0
    except (AssertionError, SchedulerError, ValueError) as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
