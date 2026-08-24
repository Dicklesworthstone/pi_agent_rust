#!/usr/bin/env python3
"""Build deterministic perf artifact staging manifests."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

from preflight_budget_inputs import (
    CONTEXT_BUDGET_ARTIFACTS,
    DEFAULT_EVIDENCE_CACHE_TTL_HOURS,
    DEFAULT_MAX_ARTIFACT_AGE_HOURS,
    EVIDENCE_CACHE_ENTRY_SCHEMA,
    EVIDENCE_CACHE_SCHEMA,
    EXTENSION_BLOCKER_BEAD,
    ArtifactGroup,
    EvidenceCacheContext,
    artifact_groups,
    current_git_commit,
    current_host_fingerprint,
    current_toolchain,
    context_criterion_relative,
    evidence_cache_for_group,
    inspect_direct_artifact,
    iso_now,
    load_evidence_cache_entries,
    parse_utc_timestamp,
    read_json,
    resolve_cache_dir,
    resolve_target_dir,
    sha256_file,
)


STAGING_SCHEMA = "pi.perf.artifact_staging_manifest.v1"
STAGING_ENTRY_SCHEMA = "pi.perf.artifact_staging_entry.v1"


def utc_now() -> datetime:
    return datetime.now(timezone.utc)


def repo_root_from_script() -> Path:
    return Path(__file__).resolve().parents[2]


def artifact_schema(path: Path) -> str | None:
    if path.suffix == ".json":
        payload = read_json(path)
        value = payload.get("schema") if payload else None
        return value if isinstance(value, str) else None
    if path.suffix == ".jsonl":
        try:
            with path.open("r", encoding="utf-8") as handle:
                for line in handle:
                    line = line.strip()
                    if not line:
                        continue
                    payload = json.loads(line)
                    if isinstance(payload, dict) and isinstance(payload.get("schema"), str):
                        return payload["schema"]
                    return None
        except (OSError, json.JSONDecodeError):
            return None
    return None


def mtime_utc(path: Path) -> str | None:
    try:
        modified = datetime.fromtimestamp(path.stat().st_mtime, tz=timezone.utc)
    except OSError:
        return None
    return modified.isoformat().replace("+00:00", "Z")


def remote_source_path(
    candidate: Path, target_dir: Path, remote_target_dir: Path | None
) -> tuple[str, bool]:
    if remote_target_dir is None:
        return str(candidate), True
    try:
        relative = candidate.resolve().relative_to(target_dir.resolve())
    except ValueError:
        return str(candidate), False
    return str(remote_target_dir / relative), False


def staged_report_path(
    candidate: Path, repo_root: Path, local_results_dir: Path | None
) -> Path | None:
    if local_results_dir is None:
        return None
    reports_root = repo_root / "tests/perf/reports"
    try:
        relative = candidate.resolve().relative_to(reports_root.resolve())
    except ValueError:
        return None
    return local_results_dir / "perf_reports" / relative


def artifact_entry(
    group: ArtifactGroup,
    candidate: Path,
    repo_root: Path,
    target_dir: Path,
    local_results_dir: Path | None,
    remote_target_dir: Path | None,
    max_age_hours: float,
    now: datetime,
    runner_mode: str,
    expected_git_commit: str,
    expected_correlation_id: str | None,
) -> dict[str, Any]:
    exists = candidate.is_file()
    inspection = (
        inspect_direct_artifact(
            candidate,
            max_age_hours,
            now,
            expected_git_commit=expected_git_commit,
            expected_correlation_id=expected_correlation_id,
        )
        if exists
        else None
    )
    is_fresh = bool(inspection and inspection["is_fresh"])
    status = "present" if is_fresh else "stale" if exists else "missing"
    retrieval_status = {
        "present": "retrieved",
        "stale": "stale_after_run",
        "missing": "missing_after_run",
    }[status]
    source_path, inferred_remote_source = remote_source_path(candidate, target_dir, remote_target_dir)
    staged_path = staged_report_path(candidate, repo_root, local_results_dir)
    staged_path_str = str(staged_path) if staged_path is not None and staged_path.is_file() else None

    try:
        size_bytes = candidate.stat().st_size if exists else None
    except OSError:
        size_bytes = None

    return {
        "schema": STAGING_ENTRY_SCHEMA,
        "contract_id": group.contract_id,
        "budget_names": list(group.budget_names),
        "required": True,
        "evidence_source": "direct",
        "reused_evidence": False,
        "status": status,
        "retrieval_status": retrieval_status,
        "reason": group.reason,
        "remote_source_path": source_path,
        "remote_source_path_inferred": inferred_remote_source,
        "source_path": str(candidate),
        "local_retrieved_path": str(candidate) if exists else None,
        "local_staged_path": staged_path_str,
        "size_bytes": size_bytes,
        "mtime_utc": mtime_utc(candidate) if exists else None,
        "age_hours": inspection.get("age_hours") if inspection else None,
        "mtime_age_hours": inspection.get("mtime_age_hours") if inspection else None,
        "max_age_hours": max_age_hours,
        "sha256": sha256_file(candidate) if exists else None,
        "artifact_schema": artifact_schema(candidate) if exists else None,
        "freshness_basis": inspection.get("freshness_basis") if inspection else None,
        "freshness_reason": inspection.get("freshness_reason") if inspection else "missing",
        "freshness_failures": inspection.get("freshness_failures", []) if inspection else [],
        "embedded_timestamp": inspection.get("embedded_timestamp") if inspection else None,
        "source_commit": inspection.get("source_commit") if inspection else None,
        "source_dirty": inspection.get("source_dirty") if inspection else None,
        "run_id": inspection.get("run_id") if inspection else None,
        "correlation_id": inspection.get("correlation_id") if inspection else None,
        "runner_mode": runner_mode,
        "suggested_commands": list(group.suggested_commands),
        "blocker": group.blocker,
    }


def run_id_from_env() -> str | None:
    for key in ("PERF_CLAIM_CORRELATION_ID", "CI_CORRELATION_ID", "PI_PERF_CORRELATION_ID"):
        value = os.environ.get(key, "").strip()
        if value:
            return value
    return None


def cache_artifact_entry(
    group: ArtifactGroup,
    cached: dict[str, Any],
    runner_mode: str,
) -> dict[str, Any]:
    return {
        "schema": STAGING_ENTRY_SCHEMA,
        "contract_id": group.contract_id,
        "budget_names": list(group.budget_names),
        "required": True,
        "evidence_source": "cache",
        "reused_evidence": True,
        "status": "present",
        "retrieval_status": "reused_from_cache",
        "reason": group.reason,
        "remote_source_path": cached.get("artifact_path") or cached.get("path"),
        "remote_source_path_inferred": False,
        "source_path": cached.get("artifact_path") or cached.get("path"),
        "local_retrieved_path": cached.get("cache_artifact_path") or cached.get("path"),
        "local_staged_path": None,
        "size_bytes": cached.get("size_bytes"),
        "mtime_utc": None,
        "age_hours": cached.get("age_hours"),
        "max_age_hours": cached.get("max_age_hours"),
        "sha256": cached.get("sha256"),
        "artifact_schema": cached.get("artifact_schema"),
        "runner_mode": runner_mode,
        "suggested_commands": list(group.suggested_commands),
        "blocker": group.blocker,
        "cache_index_path": cached.get("cache_index_path"),
        "cache_artifact_path": cached.get("cache_artifact_path"),
        "git_commit": cached.get("git_commit"),
        "build_profile": cached.get("build_profile"),
        "run_id": cached.get("run_id"),
        "correlation_id": cached.get("correlation_id"),
        "cache_created_at": cached.get("created_at"),
        "cache_ttl_hours": cached.get("ttl_hours"),
        "cache_expires_at": cached.get("expires_at"),
    }


def cache_artifact_destination(cache_dir: Path, contract_id: str, source_path: Path, sha256: str) -> Path:
    suffix = source_path.suffix or ".artifact"
    safe_contract = "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in contract_id)
    return cache_dir / "artifacts" / safe_contract / f"{sha256}{suffix}"


def update_evidence_cache_index(
    *,
    context: EvidenceCacheContext,
    groups: list[ArtifactGroup],
    entries: list[dict[str, Any]],
    run_id: str | None,
    now: datetime,
) -> dict[str, Any]:
    index_path = context.cache_dir / "index.json"
    status: dict[str, Any] = {
        "schema": EVIDENCE_CACHE_SCHEMA,
        "cache_dir": str(context.cache_dir),
        "index_path": str(index_path),
        "update_enabled": True,
        "written_entry_count": 0,
        "skipped_entry_count": 0,
    }
    if not run_id:
        status["detail"] = "skipped_missing_run_id"
        status["skipped_entry_count"] = sum(
            1
            for entry in entries
            if entry.get("evidence_source") == "direct" and entry.get("status") == "present"
        )
        return status

    group_by_contract = {group.contract_id: group for group in groups}
    existing_entries, existing_status = load_evidence_cache_entries(context.cache_dir)
    if not existing_status.get("index_valid"):
        existing_entries = []

    next_entries = list(existing_entries)
    seen_keys = {
        (
            entry.get("contract_id"),
            entry.get("git_commit"),
            entry.get("build_profile"),
            entry.get("sha256"),
        )
        for entry in next_entries
        if isinstance(entry, dict)
    }

    toolchain = current_toolchain()
    host_fingerprint = current_host_fingerprint()

    for staging_entry in entries:
        if staging_entry.get("evidence_source") != "direct" or staging_entry.get("status") != "present":
            continue
        contract_id = str(staging_entry.get("contract_id", "")).strip()
        group = group_by_contract.get(contract_id)
        source_path_raw = staging_entry.get("local_retrieved_path")
        sha = staging_entry.get("sha256")
        if group is None or not isinstance(source_path_raw, str) or not isinstance(sha, str):
            status["skipped_entry_count"] += 1
            continue

        source_path = Path(source_path_raw)
        if not source_path.is_file():
            status["skipped_entry_count"] += 1
            continue

        created_at = staging_entry.get("embedded_timestamp")
        parsed_created_at = parse_utc_timestamp(created_at)
        if parsed_created_at is None:
            parsed_created_at = now
            created_at = now.isoformat().replace("+00:00", "Z")
        expires_at = (parsed_created_at + timedelta(hours=context.max_ttl_hours)).isoformat().replace(
            "+00:00", "Z"
        )

        cache_path = cache_artifact_destination(context.cache_dir, contract_id, source_path, sha)
        cache_path.parent.mkdir(parents=True, exist_ok=True)
        if not cache_path.exists():
            shutil.copy2(source_path, cache_path)

        cache_entry = {
            "schema": EVIDENCE_CACHE_ENTRY_SCHEMA,
            "contract_id": contract_id,
            "budget_names": staging_entry.get("budget_names", []),
            "artifact_path": source_path_raw,
            "cache_artifact_path": str(cache_path),
            "command": group.suggested_commands[0] if group.suggested_commands else "unknown",
            "git_commit": context.git_commit,
            "toolchain": toolchain,
            "host_fingerprint": host_fingerprint,
            "build_profile": context.build_profile,
            "run_id": staging_entry.get("run_id") or run_id,
            "correlation_id": staging_entry.get("correlation_id") or run_id,
            "source_commit": staging_entry.get("source_commit"),
            "source_dirty": staging_entry.get("source_dirty"),
            "sha256": sha,
            "artifact_schema": staging_entry.get("artifact_schema"),
            "created_at": created_at,
            "ttl_hours": context.max_ttl_hours,
            "expires_at": expires_at,
        }
        key = (
            cache_entry["contract_id"],
            cache_entry["git_commit"],
            cache_entry["build_profile"],
            cache_entry["sha256"],
        )
        if key in seen_keys:
            continue
        seen_keys.add(key)
        next_entries.append(cache_entry)
        status["written_entry_count"] += 1

    context.cache_dir.mkdir(parents=True, exist_ok=True)
    payload = {
        "schema": EVIDENCE_CACHE_SCHEMA,
        "generated_at": iso_now(),
        "entries": next_entries,
    }
    index_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    status["entry_count"] = len(next_entries)
    return status


def build_staging_manifest(
    repo_root: Path,
    target_dir: Path,
    local_results_dir: Path | None,
    remote_target_dir: Path | None,
    max_age_hours: float,
    now: datetime,
    runner_mode: str,
    cache_dir: Path,
    cache_git_commit: str,
    cache_profile: str,
    cache_ttl_hours: float,
    run_id: str | None,
    expected_correlation_id: str | None,
    update_evidence_cache: bool,
) -> dict[str, Any]:
    groups = artifact_groups(repo_root, target_dir)
    cache_context = EvidenceCacheContext(
        repo_root=repo_root,
        target_dir=target_dir,
        cache_dir=cache_dir,
        git_commit=cache_git_commit,
        build_profile=cache_profile,
        max_ttl_hours=cache_ttl_hours,
    )
    cache_entries, cache_status = load_evidence_cache_entries(cache_dir)
    entries: list[dict[str, Any]] = []
    rejected_cache_entries: list[dict[str, Any]] = []
    blockers: list[dict[str, Any]] = []
    present_required = 0
    stale_required = 0
    missing_required = 0
    cache_reused_required = 0

    for group in groups:
        group_entries = [
            artifact_entry(
                group=group,
                candidate=candidate,
                repo_root=repo_root,
                target_dir=target_dir,
                local_results_dir=local_results_dir,
                remote_target_dir=remote_target_dir,
                max_age_hours=max_age_hours,
                now=now,
                runner_mode=runner_mode,
                expected_git_commit=cache_git_commit,
                expected_correlation_id=expected_correlation_id,
            )
            for candidate in group.candidates
        ]
        cache_fresh, cache_rejected = evidence_cache_for_group(
            cache_entries,
            group,
            cache_context,
            now,
        )
        rejected_cache_entries.extend(cache_rejected)
        has_direct_present = any(entry["status"] == "present" for entry in group_entries)
        if not has_direct_present and cache_fresh:
            group_entries.extend(
                cache_artifact_entry(group, cached, runner_mode) for cached in cache_fresh
            )
            cache_reused_required += 1
        entries.extend(group_entries)
        has_present = any(entry["status"] == "present" for entry in group_entries)
        has_stale = any(entry["status"] == "stale" for entry in group_entries)
        group_status = "present" if has_present else "stale" if has_stale else "missing"
        if group_status == "present":
            present_required += 1
        elif group_status == "stale":
            stale_required += 1
        else:
            missing_required += 1
        if group_status != "present":
            blockers.append(
                {
                    "contract_id": group.contract_id,
                    "budget_names": list(group.budget_names),
                    "status": group_status,
                    "reason": group.reason,
                    "expected_paths": [str(path) for path in group.expected_outputs],
                    "candidate_paths": [str(path) for path in group.candidates],
                    "suggested_commands": list(group.suggested_commands),
                    "blocker": group.blocker,
                }
            )

    cache_update_status = (
        update_evidence_cache_index(
            context=cache_context,
            groups=groups,
            entries=entries,
            run_id=run_id,
            now=now,
        )
        if update_evidence_cache
        else {
            "schema": EVIDENCE_CACHE_SCHEMA,
            "cache_dir": str(cache_dir),
            "index_path": str(cache_dir / "index.json"),
            "update_enabled": False,
        }
    )
    cache_status.update(
        {
            "expected_git_commit": cache_context.git_commit,
            "expected_build_profile": cache_context.build_profile,
            "max_ttl_hours": cache_context.max_ttl_hours,
            "accepted_entry_count": sum(
                1
                for entry in entries
                if entry.get("evidence_source") == "cache" and entry.get("status") == "present"
            ),
            "rejected_entry_count": len(rejected_cache_entries),
            "update": cache_update_status,
        }
    )
    if update_evidence_cache:
        cache_status["index_exists"] = index_path_exists = (cache_dir / "index.json").is_file()
        cache_status["index_valid"] = index_path_exists
        if "entry_count" in cache_update_status:
            cache_status["entry_count"] = cache_update_status["entry_count"]
        if cache_update_status.get("written_entry_count", 0) > 0:
            cache_status["detail"] = "cache index updated"

    status = "ready" if missing_required == 0 and stale_required == 0 else "blocked"
    return {
        "schema": STAGING_SCHEMA,
        "generated_at": iso_now(),
        "repo_root": str(repo_root),
        "cargo_target_dir": str(target_dir),
        "remote_target_dir": str(remote_target_dir) if remote_target_dir is not None else None,
        "remote_source_path_mode": "explicit" if remote_target_dir is not None else "inferred_from_local_target",
        "local_results_dir": str(local_results_dir) if local_results_dir is not None else None,
        "max_artifact_age_hours": max_age_hours,
        "runner_mode": runner_mode,
        "summary": {
            "status": status,
            "required_contract_count": len(groups),
            "present_required_count": present_required,
            "stale_required_count": stale_required,
            "missing_required_count": missing_required,
            "cache_reused_required_count": cache_reused_required,
            "entry_count": len(entries),
        },
        "evidence_cache": cache_status,
        "entries": entries,
        "rejected_evidence_cache_entries": rejected_cache_entries,
        "blockers": blockers,
        "safety_notes": [
            "Do not refresh tests/perf/reports/budget_summary.json while this manifest status is blocked.",
            "For RCH report generation, copy ready artifacts into a repo-visible evidence root and run perf_budgets with PERF_EVIDENCE_DIR pointing at that root.",
            "Cached perf evidence is reused only after commit, build profile, TTL, lineage, schema, and checksum validation pass; reused entries are labeled evidence_source=cache.",
            "For RCH runs, remote_source_path is explicit only when PERF_REMOTE_TARGET_DIR is provided; "
            "otherwise it records the local post-RCH source path.",
        ],
    }


def write_fixture(root: Path, include_policy: bool) -> None:
    (root / "tests/perf/reports").mkdir(parents=True)
    (root / "target/criterion/ext_load_init/load_init_cold/hello/new").mkdir(parents=True)
    (root / "target/criterion/ext_protocol/parse_and_validate/log/new").mkdir(parents=True)
    (root / "target/perf/perf").mkdir(parents=True)
    (root / "target/release").mkdir(parents=True)
    (root / "target/perf/results").mkdir(parents=True)
    if include_policy:
        (root / "target/criterion/ext_policy/evaluate/safe/new").mkdir(parents=True)

    estimate_paths = [
        root / "target/criterion/startup/version/warm/new/estimates.json",
        root / "target/criterion/ext_load_init/load_init_cold/hello/new/estimates.json",
        root / "target/criterion/ext_protocol/parse_and_validate/log/new/estimates.json",
    ]
    estimate_paths.extend(
        root / "target" / context_criterion_relative(bench_name)
        for _budget_name, bench_name in CONTEXT_BUDGET_ARTIFACTS
    )
    if include_policy:
        estimate_paths.append(root / "target/criterion/ext_policy/evaluate/safe/new/estimates.json")
    for path in estimate_paths:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('{"mean":{"point_estimate":1000.0}}\n', encoding="utf-8")

    (root / "target/perf/perf/pijs_workload_perf.jsonl").write_text(
        '{"schema":"pi.perf.workload.v1","tool_calls_per_iteration":1}\n',
        encoding="utf-8",
    )
    (root / "target/release/pi").write_bytes(b"binary")
    (root / "tests/perf/reports/extension_benchmark_stratification.json").write_text(
        '{"schema":"pi.perf.extension_benchmark_stratification.v1"}',
        encoding="utf-8",
    )
    (root / "target/perf/results/phase1_matrix_validation.json").write_text(
        '{"schema":"pi.perf.phase1_matrix_validation.v1"}',
        encoding="utf-8",
    )
    context_budget_path = root / "target/perf/context_intelligence/perf_budget.json"
    context_budget_path.parent.mkdir(parents=True, exist_ok=True)
    context_budget_path.write_text(
        json.dumps(
            {
                "schema": "pi.semantic_context.performance_budget.v1",
                "environment": {
                    "cargo_target_dir": str(root / "target"),
                    "tmpdir": str(root / "tmp"),
                },
                "host": {"os": "self-test", "arch": "x86_64"},
                "determinism": {
                    "randomized_file_order_checked": True,
                    "matched": True,
                },
                "cache_hit_miss": {
                    "cold_graph_build": "fresh_fixture",
                    "warm_graph_build": "same_workspace_rebuild",
                    "incremental_update": "single_file_rebuild",
                },
                "metrics": {
                    "context_graph_build_cold_ms": {"p95_ms": 1.0},
                    "context_graph_build_warm_ms": {"p95_ms": 1.0},
                    "context_incremental_update_ms": {"p95_ms": 1.0},
                    "context_planning_ms": {"p95_ms": 1.0},
                    "context_bundle_serialization_ms": {"p95_ms": 1.0},
                    "context_bundle_estimated_bytes": {"bytes": 1024.0},
                },
            }
        ),
        encoding="utf-8",
    )


def run_self_test() -> int:
    def staging_args(
        root: Path,
        *,
        cache_dir: Path | None = None,
        update_cache: bool = False,
        expected_correlation_id: str | None = None,
    ) -> dict[str, Any]:
        return {
            "repo_root": root,
            "target_dir": root / "target",
            "local_results_dir": root / "run/results",
            "remote_target_dir": Path("/remote/pi-agent-target"),
            "max_age_hours": 24.0,
            "now": utc_now(),
            "runner_mode": "rch",
            "cache_dir": cache_dir or root / "target/perf/evidence_cache",
            "cache_git_commit": "test-commit",
            "cache_profile": "perf",
            "cache_ttl_hours": 24.0,
            "run_id": "self-test-run",
            "expected_correlation_id": expected_correlation_id,
            "update_evidence_cache": update_cache,
        }

    def write_cache_index(root: Path) -> Path:
        cache_dir = root / "target/perf/evidence_cache"
        artifact_path = cache_dir / "artifacts/extension_criterion_policy/estimates.json"
        artifact_path.parent.mkdir(parents=True, exist_ok=True)
        artifact_path.write_text('{"mean":{"point_estimate":1000.0}}\n', encoding="utf-8")
        entry = {
            "schema": EVIDENCE_CACHE_ENTRY_SCHEMA,
            "contract_id": "extension_criterion_policy",
            "artifact_path": str(root / "target/criterion/ext_policy/evaluate/safe/new/estimates.json"),
            "cache_artifact_path": str(artifact_path),
            "command": "rch exec -- cargo bench --bench extension_budget_inputs --profile perf ext_policy",
            "git_commit": "test-commit",
            "toolchain": "rustc 1.85.0-test",
            "host_fingerprint": {"hostname": "self-test", "cpu_count": 1},
            "build_profile": "perf",
            "run_id": "run-123",
            "correlation_id": "corr-123",
            "sha256": sha256_file(artifact_path),
            "artifact_schema": None,
            "created_at": iso_now(),
            "ttl_hours": 24.0,
        }
        (cache_dir / "index.json").write_text(
            json.dumps({"schema": EVIDENCE_CACHE_SCHEMA, "entries": [entry]}),
            encoding="utf-8",
        )
        return cache_dir

    ok_root = Path(tempfile.mkdtemp(prefix="pi-perf-staging-ok-"))
    write_fixture(ok_root, include_policy=True)
    ok_manifest = build_staging_manifest(**staging_args(ok_root, update_cache=True))
    assert ok_manifest["summary"]["status"] == "ready", ok_manifest
    assert ok_manifest["evidence_cache"]["update"]["written_entry_count"] >= 1, ok_manifest
    policy_entries = [
        entry
        for entry in ok_manifest["entries"]
        if entry["contract_id"] == "extension_criterion_policy"
        and entry["status"] == "present"
    ]
    assert policy_entries, ok_manifest
    assert (
        policy_entries[0]["remote_source_path"]
        == "/remote/pi-agent-target/criterion/ext_policy/evaluate/safe/new/estimates.json"
    ), policy_entries[0]
    assert policy_entries[0]["retrieval_status"] == "retrieved", policy_entries[0]

    stale_provenance_root = Path(tempfile.mkdtemp(prefix="pi-perf-staging-stale-lineage-"))
    write_fixture(stale_provenance_root, include_policy=True)
    stale_phase1 = stale_provenance_root / "target/perf/results/phase1_matrix_validation.json"
    stale_phase1.write_text(
        json.dumps(
            {
                "schema": "pi.perf.phase1_matrix_validation.v1",
                "generated_at": "2000-01-01T00:00:00Z",
                "run_id": "self-test-run",
                "correlation_id": "self-test-run",
            }
        ),
        encoding="utf-8",
    )
    stale_manifest = build_staging_manifest(
        **staging_args(
            stale_provenance_root,
            update_cache=True,
            expected_correlation_id="self-test-run",
        )
    )
    assert stale_manifest["summary"]["status"] == "blocked", stale_manifest
    assert any(
        entry["contract_id"] == "phase1_matrix_validation"
        and entry["status"] == "stale"
        and entry["freshness_reason"] == "embedded_timestamp_stale"
        and entry["mtime_age_hours"] < 1.0
        for entry in stale_manifest["entries"]
    ), stale_manifest
    stale_cache_entries, _ = load_evidence_cache_entries(
        stale_provenance_root / "target/perf/evidence_cache"
    )
    assert not any(
        entry.get("contract_id") == "phase1_matrix_validation"
        for entry in stale_cache_entries
    ), stale_cache_entries

    fresh_provenance_root = Path(tempfile.mkdtemp(prefix="pi-perf-staging-fresh-lineage-"))
    write_fixture(fresh_provenance_root, include_policy=True)
    fresh_phase1 = fresh_provenance_root / "target/perf/results/phase1_matrix_validation.json"
    fresh_phase1.write_text(
        json.dumps(
            {
                "schema": "pi.perf.phase1_matrix_validation.v1",
                "generated_at": iso_now(),
                "source_commit": "test-commit",
                "source_dirty": False,
                "run_id": "timestamp-derived-run",
                "correlation_id": "self-test-run",
            }
        ),
        encoding="utf-8",
    )
    old_mtime = utc_now().timestamp() - (48.0 * 3600.0)
    os.utime(fresh_phase1, (old_mtime, old_mtime))
    fresh_manifest = build_staging_manifest(
        **staging_args(
            fresh_provenance_root,
            update_cache=True,
            expected_correlation_id="self-test-run",
        )
    )
    assert fresh_manifest["summary"]["status"] == "ready", fresh_manifest
    assert any(
        entry["contract_id"] == "phase1_matrix_validation"
        and entry["status"] == "present"
        and entry["freshness_basis"] == "embedded_timestamp"
        and entry["mtime_age_hours"] > 24.0
        for entry in fresh_manifest["entries"]
    ), fresh_manifest
    fresh_cache_entries, _ = load_evidence_cache_entries(
        fresh_provenance_root / "target/perf/evidence_cache"
    )
    fresh_phase1_entry = next(
        entry
        for entry in fresh_manifest["entries"]
        if entry["contract_id"] == "phase1_matrix_validation"
        and entry["status"] == "present"
    )
    fresh_phase1_cache = next(
        entry
        for entry in fresh_cache_entries
        if entry.get("contract_id") == "phase1_matrix_validation"
    )
    assert fresh_phase1_cache["created_at"] == fresh_phase1_entry["embedded_timestamp"], (
        fresh_phase1_cache
    )
    assert fresh_phase1_cache["run_id"] == "timestamp-derived-run", fresh_phase1_cache
    assert fresh_phase1_cache["correlation_id"] == "self-test-run", fresh_phase1_cache

    blocked_root = Path(tempfile.mkdtemp(prefix="pi-perf-staging-blocked-"))
    write_fixture(blocked_root, include_policy=False)
    blocked_manifest = build_staging_manifest(**staging_args(blocked_root))
    assert blocked_manifest["summary"]["status"] == "blocked", blocked_manifest
    assert any(
        entry["contract_id"] == "extension_criterion_policy"
        and entry["retrieval_status"] == "missing_after_run"
        for entry in blocked_manifest["entries"]
    ), blocked_manifest
    assert any(
        blocker["contract_id"] == "extension_criterion_policy"
        and blocker["blocker"] == EXTENSION_BLOCKER_BEAD
        for blocker in blocked_manifest["blockers"]
    ), blocked_manifest

    cached_root = Path(tempfile.mkdtemp(prefix="pi-perf-staging-cache-"))
    write_fixture(cached_root, include_policy=False)
    cache_dir = write_cache_index(cached_root)
    cached_manifest = build_staging_manifest(**staging_args(cached_root, cache_dir=cache_dir))
    assert cached_manifest["summary"]["status"] == "ready", cached_manifest
    assert cached_manifest["summary"]["cache_reused_required_count"] == 1, cached_manifest
    assert any(
        entry["contract_id"] == "extension_criterion_policy"
        and entry["evidence_source"] == "cache"
        and entry["retrieval_status"] == "reused_from_cache"
        for entry in cached_manifest["entries"]
    ), cached_manifest
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", help="Repository root. Defaults to this script's repo.")
    parser.add_argument(
        "--cargo-target-dir",
        help="Cargo target directory to inspect. Defaults to CARGO_TARGET_DIR or ./target.",
    )
    parser.add_argument("--local-results-dir", help="Perf run results directory.")
    parser.add_argument("--remote-target-dir", help="Remote CARGO_TARGET_DIR prefix for RCH source paths.")
    parser.add_argument("--runner-mode", default="unknown", help="Resolved cargo runner mode.")
    parser.add_argument("--output", help="Manifest output path. Defaults to stdout.")
    parser.add_argument(
        "--evidence-cache-dir",
        help="Perf evidence cache directory. Defaults to PI_PERF_EVIDENCE_CACHE_DIR or target/perf/evidence_cache.",
    )
    parser.add_argument(
        "--cache-ttl-hours",
        type=float,
        default=float(
            os.environ.get("PI_PERF_EVIDENCE_CACHE_TTL_HOURS", DEFAULT_EVIDENCE_CACHE_TTL_HOURS)
        ),
        help="Maximum reusable cache TTL in hours.",
    )
    parser.add_argument(
        "--cache-profile",
        help="Expected cached evidence build profile. Defaults to PERF_PROFILE, CARGO_PROFILE, or perf.",
    )
    parser.add_argument(
        "--cache-git-commit",
        help="Expected cached evidence git commit. Defaults to PI_PERF_GIT_COMMIT or current HEAD.",
    )
    parser.add_argument("--run-id", help="Run/correlation ID to store when updating the evidence cache.")
    parser.add_argument(
        "--expected-correlation-id",
        help="When set, embedded correlation_id (or run_id fallback) must match this value.",
    )
    parser.add_argument(
        "--update-evidence-cache",
        action="store_true",
        help="Copy present direct artifacts into the evidence cache and refresh index.json.",
    )
    parser.add_argument(
        "--max-age-hours",
        type=float,
        default=float(
            os.environ.get("PI_PERF_MAX_ARTIFACT_AGE_HOURS", DEFAULT_MAX_ARTIFACT_AGE_HOURS)
        ),
        help="Maximum accepted artifact age in hours.",
    )
    parser.add_argument("--self-test", action="store_true", help="Run disposable self-tests.")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.self_test:
        return run_self_test()

    repo_root = Path(args.repo_root).resolve() if args.repo_root else repo_root_from_script()
    target_dir = resolve_target_dir(repo_root, args.cargo_target_dir or os.environ.get("CARGO_TARGET_DIR"))
    local_results_dir = Path(args.local_results_dir).resolve() if args.local_results_dir else None
    remote_target_dir = Path(args.remote_target_dir).expanduser() if args.remote_target_dir else None
    cache_dir = resolve_cache_dir(
        target_dir,
        args.evidence_cache_dir or os.environ.get("PI_PERF_EVIDENCE_CACHE_DIR"),
    )
    manifest = build_staging_manifest(
        repo_root=repo_root,
        target_dir=target_dir,
        local_results_dir=local_results_dir,
        remote_target_dir=remote_target_dir,
        max_age_hours=args.max_age_hours,
        now=utc_now(),
        runner_mode=args.runner_mode,
        cache_dir=cache_dir,
        cache_git_commit=args.cache_git_commit
        or os.environ.get("PI_PERF_GIT_COMMIT")
        or current_git_commit(repo_root),
        cache_profile=args.cache_profile
        or os.environ.get("PERF_PROFILE")
        or os.environ.get("CARGO_PROFILE")
        or "perf",
        cache_ttl_hours=args.cache_ttl_hours,
        run_id=args.run_id or run_id_from_env(),
        expected_correlation_id=args.expected_correlation_id
        or os.environ.get("PI_PERF_EXPECTED_CORRELATION_ID"),
        update_evidence_cache=args.update_evidence_cache,
    )
    text = json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    if args.output:
        output = Path(args.output).expanduser()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(text, encoding="utf-8")
    else:
        print(text, end="")
    return 0 if manifest["summary"]["status"] == "ready" else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
