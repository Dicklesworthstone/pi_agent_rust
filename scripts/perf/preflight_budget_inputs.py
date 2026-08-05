#!/usr/bin/env python3
"""Preflight performance budget evidence inputs.

This is a fast, read-only check for agents before starting expensive perf runs.
It mirrors the artifact paths used by tests/perf_budgets.rs and emits stable
JSON describing missing or stale inputs, suggested RCH commands, and known
blocker context.
"""

from __future__ import annotations

import argparse
import glob
import hashlib
import json
import math
import os
import platform
import re
import shutil
import socket
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


SCHEMA = "pi.perf.budget_preflight.v1"
EVIDENCE_CACHE_SCHEMA = "pi.perf.evidence_cache.v1"
EVIDENCE_CACHE_ENTRY_SCHEMA = "pi.perf.evidence_cache_entry.v1"
HOST_TOPOLOGY_SCHEMA = "pi.perf.host_topology_fingerprint.v1"
DEFAULT_MAX_ARTIFACT_AGE_HOURS = 24.0
DEFAULT_EVIDENCE_CACHE_TTL_HOURS = 168.0
EXTENSION_BLOCKER_BEAD = "bd-2zcs5.51"


@dataclass(frozen=True)
class BudgetContract:
    name: str
    category: str
    methodology: str
    ci_enforced: bool


@dataclass(frozen=True)
class ArtifactGroup:
    contract_id: str
    budget_names: tuple[str, ...]
    candidates: tuple[Path, ...]
    suggested_commands: tuple[str, ...]
    reason: str
    expected_outputs: tuple[Path, ...]
    blocker: str | None = None


@dataclass(frozen=True)
class EvidenceCacheContext:
    repo_root: Path
    target_dir: Path
    cache_dir: Path
    git_commit: str
    build_profile: str
    max_ttl_hours: float


def utc_now() -> datetime:
    return datetime.now(timezone.utc)


def iso_now() -> str:
    return utc_now().isoformat().replace("+00:00", "Z")


def repo_root_from_script() -> Path:
    return Path(__file__).resolve().parents[2]


def resolve_target_dir(repo_root: Path, raw_target_dir: str | None) -> Path:
    if raw_target_dir:
        target_dir = Path(raw_target_dir).expanduser()
        if target_dir.is_absolute():
            return target_dir
        return repo_root / target_dir
    return repo_root / "target"


def resolve_env_path(repo_root: Path, raw_path: str) -> Path | None:
    raw_path = raw_path.strip()
    if not raw_path:
        return None
    path = Path(raw_path).expanduser()
    if path.is_absolute():
        return path
    return repo_root / path


def dedupe_paths(paths: list[Path] | tuple[Path, ...]) -> tuple[Path, ...]:
    deduped: list[Path] = []
    seen: set[str] = set()
    for path in paths:
        key = str(path)
        if key in seen:
            continue
        seen.add(key)
        deduped.append(path)
    return tuple(deduped)


def perf_evidence_dirs(repo_root: Path) -> tuple[Path, ...]:
    dirs: list[Path] = []
    raw_single = os.environ.get("PERF_EVIDENCE_DIR")
    if raw_single:
        path = resolve_env_path(repo_root, raw_single)
        if path is not None:
            dirs.append(path)
    raw_many = os.environ.get("PERF_EVIDENCE_DIRS")
    if raw_many:
        for raw_path in raw_many.split(os.pathsep):
            path = resolve_env_path(repo_root, raw_path)
            if path is not None:
                dirs.append(path)
    return dedupe_paths(dirs)


def evidence_then_target_paths(
    repo_root: Path,
    target_dir: Path,
    evidence_relative_paths: tuple[str, ...],
    target_relative_paths: tuple[str, ...],
) -> tuple[Path, ...]:
    paths: list[Path] = []
    for evidence_dir in perf_evidence_dirs(repo_root):
        paths.extend(evidence_dir / relative for relative in evidence_relative_paths)
    paths.extend(target_dir / relative for relative in target_relative_paths)
    return dedupe_paths(paths)


def rel_or_abs(repo_root: Path, path: Path) -> str:
    try:
        return path.resolve().relative_to(repo_root.resolve()).as_posix()
    except ValueError:
        return str(path)


def sha256_file(path: Path) -> str | None:
    try:
        digest = hashlib.sha256()
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest()
    except OSError:
        return None


def current_git_commit(repo_root: Path) -> str:
    try:
        result = subprocess.run(
            ["git", "-C", str(repo_root), "rev-parse", "HEAD"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"
    commit = result.stdout.strip()
    return commit if result.returncode == 0 and commit else "unknown"


def current_toolchain() -> str:
    try:
        result = subprocess.run(
            ["rustc", "-Vv"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"
    toolchain = result.stdout.strip()
    return toolchain if result.returncode == 0 and toolchain else "unknown"


def current_host_fingerprint() -> dict[str, Any]:
    return build_host_topology_fingerprint()["host_fingerprint"]


def read_text_optional(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return None


def parse_meminfo_total_mb(raw: str | None) -> int | None:
    if raw is None:
        return None
    for line in raw.splitlines():
        if not line.startswith("MemTotal:"):
            continue
        parts = line.split()
        if len(parts) < 2:
            return None
        try:
            return max(1, int(parts[1]) // 1024)
        except ValueError:
            return None
    return None


def parse_cpuinfo(raw: str | None) -> tuple[int | None, str | None]:
    if raw is None:
        return None, None
    cpu_count = 0
    cpu_model: str | None = None
    for line in raw.splitlines():
        if line.startswith("processor"):
            cpu_count += 1
        if cpu_model is None and line.startswith("model name"):
            _, _, value = line.partition(":")
            cpu_model = value.strip() or None
    return (cpu_count or None), cpu_model


def parse_cpu_max(raw: str | None) -> tuple[float | None, str | None]:
    if raw is None:
        return None, None
    parts = raw.split()
    if len(parts) != 2:
        return None, "malformed_cpu_max"
    quota_raw, period_raw = parts
    if quota_raw == "max":
        return None, None
    try:
        quota = int(quota_raw)
        period = int(period_raw)
    except ValueError:
        return None, "malformed_cpu_max"
    if quota <= 0 or period <= 0:
        return None, "malformed_cpu_max"
    return quota / period, None


def parse_memory_max_mb(raw: str | None) -> tuple[int | None, str | None]:
    if raw is None:
        return None, None
    if raw == "max":
        return None, None
    try:
        value = int(raw)
    except ValueError:
        return None, "malformed_memory_max"
    if value <= 0:
        return None, "malformed_memory_max"
    return max(1, value // (1024 * 1024)), None


def parse_cpuset_cpu_count(raw: str | None) -> tuple[int | None, str | None]:
    if raw is None:
        return None, None
    value = raw.strip()
    if not value:
        return None, None
    cpus: set[int] = set()
    for part in value.split(","):
        part = part.strip()
        if not part:
            return None, "malformed_cpuset"
        if "-" in part:
            start_raw, end_raw = part.split("-", 1)
            try:
                start = int(start_raw)
                end = int(end_raw)
            except ValueError:
                return None, "malformed_cpuset"
            if start < 0 or end < start:
                return None, "malformed_cpuset"
            cpus.update(range(start, end + 1))
        else:
            try:
                cpu = int(part)
            except ValueError:
                return None, "malformed_cpuset"
            if cpu < 0:
                return None, "malformed_cpuset"
            cpus.add(cpu)
    return (len(cpus) or None), None


def parse_cgroup_v2_relative_path(proc_root: Path) -> tuple[Path | None, str | None]:
    raw = read_text_optional(proc_root / "self/cgroup")
    if raw is None:
        return None, "missing_procfs_cgroup"
    for line in raw.splitlines():
        parts = line.split(":", 2)
        if len(parts) == 3 and parts[0] == "0" and parts[1] == "":
            relative = parts[2].strip().lstrip("/")
            return Path(relative) if relative else Path(), None
    return None, "missing_cgroup_v2_membership"


def numa_node_count(sys_root: Path) -> int | None:
    node_root = sys_root / "devices/system/node"
    try:
        nodes = [
            entry
            for entry in node_root.iterdir()
            if entry.is_dir() and re.fullmatch(r"node\d+", entry.name)
        ]
    except OSError:
        return None
    return len(nodes) or None


def constrained_cpu_cores(
    host_cpu_count: int | None,
    quota_cores: float | None,
    cpuset_count: int | None,
) -> int:
    candidates: list[float] = []
    if host_cpu_count is not None and host_cpu_count > 0:
        candidates.append(float(host_cpu_count))
    if quota_cores is not None and quota_cores > 0:
        candidates.append(quota_cores)
    if cpuset_count is not None and cpuset_count > 0:
        candidates.append(float(cpuset_count))
    if not candidates:
        return max(1, os.cpu_count() or 1)
    return max(1, math.floor(min(candidates)))


def constrained_memory_mb(host_mem_total_mb: int | None, memory_limit_mb: int | None) -> int:
    candidates = [value for value in (host_mem_total_mb, memory_limit_mb) if value is not None and value > 0]
    if not candidates:
        return 1
    return max(1, min(candidates))


def build_host_topology_fingerprint(
    *,
    proc_root: Path = Path("/proc"),
    sys_root: Path = Path("/sys"),
    timestamp: str | None = None,
    build_profile: str | None = None,
    pgo_mode: str | None = None,
    pgo_profile_data: str | None = None,
    pgo_allow_fallback: str | None = None,
    git_commit: str | None = None,
    git_dirty: bool | None = None,
    rust_version: str | None = None,
    cargo_runner_mode: str | None = None,
    cargo_runner_request: str | None = None,
    correlation_id: str | None = None,
) -> dict[str, Any]:
    caveats: list[str] = []
    cpuinfo = read_text_optional(proc_root / "cpuinfo")
    meminfo = read_text_optional(proc_root / "meminfo")
    if not proc_root.exists():
        caveats.append("missing_procfs")
    host_cpu_count, cpu_model = parse_cpuinfo(cpuinfo)
    if host_cpu_count is None:
        host_cpu_count = os.cpu_count()
        caveats.append("cpuinfo_unavailable")
    host_mem_total_mb = parse_meminfo_total_mb(meminfo)
    if host_mem_total_mb is None:
        caveats.append("meminfo_unavailable")

    relative_cgroup_path, cgroup_caveat = parse_cgroup_v2_relative_path(proc_root)
    if cgroup_caveat:
        caveats.append(cgroup_caveat)
    cgroup_root = sys_root / "fs/cgroup"
    cgroup_path = cgroup_root / relative_cgroup_path if relative_cgroup_path is not None else None
    if cgroup_path is not None and not cgroup_path.exists():
        caveats.append("missing_cgroupfs")

    cpu_quota_cores: float | None = None
    cpuset_count: int | None = None
    memory_limit_mb: int | None = None
    if cgroup_path is not None and cgroup_path.exists():
        cpu_quota_cores, cpu_max_caveat = parse_cpu_max(read_text_optional(cgroup_path / "cpu.max"))
        if cpu_max_caveat:
            caveats.append(cpu_max_caveat)
        cpuset_raw = read_text_optional(cgroup_path / "cpuset.cpus.effective")
        if cpuset_raw is None:
            cpuset_raw = read_text_optional(cgroup_path / "cpuset.cpus")
        cpuset_count, cpuset_caveat = parse_cpuset_cpu_count(cpuset_raw)
        if cpuset_caveat:
            caveats.append(cpuset_caveat)
        memory_limit_mb, memory_caveat = parse_memory_max_mb(
            read_text_optional(cgroup_path / "memory.max")
        )
        if memory_caveat:
            caveats.append(memory_caveat)

    effective_cpu_cores = constrained_cpu_cores(host_cpu_count, cpu_quota_cores, cpuset_count)
    effective_mem_total_mb = constrained_memory_mb(host_mem_total_mb, memory_limit_mb)
    if host_cpu_count is not None and effective_cpu_cores < host_cpu_count:
        caveats.append("container_cpu_constraint_below_host")
    if host_mem_total_mb is not None and effective_mem_total_mb < host_mem_total_mb:
        caveats.append("container_memory_constraint_below_host")

    numa_nodes = numa_node_count(sys_root)
    if numa_nodes is None:
        caveats.append("numa_topology_unavailable")

    caveats = list(dict.fromkeys(caveats))
    host_fingerprint = {
        "schema": HOST_TOPOLOGY_SCHEMA,
        "hostname": socket.gethostname(),
        "machine": platform.machine(),
        "platform": platform.platform(),
        "processor": platform.processor() or cpu_model or "unknown",
        "host_cpu_cores": host_cpu_count,
        "host_mem_total_mb": host_mem_total_mb,
        "effective_cpu_cores": effective_cpu_cores,
        "effective_mem_total_mb": effective_mem_total_mb,
        "cgroup": {
            "version": "v2" if relative_cgroup_path is not None else "unknown",
            "relative_path": str(relative_cgroup_path) if relative_cgroup_path is not None else None,
            "path": str(cgroup_path) if cgroup_path is not None else None,
            "cpu_quota_cores": cpu_quota_cores,
            "cpuset_cpu_count": cpuset_count,
            "memory_limit_mb": memory_limit_mb,
        },
        "numa": {
            "node_count": numa_nodes,
        },
        "caveats": caveats,
    }
    budget_profile = {
        "target_cpu_cores": effective_cpu_cores,
        "observed_cpu_cores": host_cpu_count or effective_cpu_cores,
        "mem_total_mb": effective_mem_total_mb,
        "host_mem_total_mb": host_mem_total_mb,
        "source": "cgroup_constrained" if any(caveat.startswith("container_") for caveat in caveats) else "host",
    }
    return {
        "schema": "pi.perf.env_fingerprint.v1",
        "host_topology_schema": HOST_TOPOLOGY_SCHEMA,
        "timestamp": timestamp or iso_now(),
        "os": platform.platform(),
        "cpu_model": cpu_model or platform.processor() or "unknown",
        "cpu_cores": effective_cpu_cores,
        "observed_cpu_cores": host_cpu_count,
        "host_cpu_cores": host_cpu_count,
        "mem_total_mb": effective_mem_total_mb,
        "host_mem_total_mb": host_mem_total_mb,
        "build_profile": build_profile,
        "pgo_mode": pgo_mode,
        "pgo_profile_data": pgo_profile_data,
        "pgo_allow_fallback": pgo_allow_fallback,
        "git_commit": git_commit,
        "git_dirty": git_dirty,
        "rust_version": rust_version,
        "cargo_runner_mode": cargo_runner_mode,
        "cargo_runner_request": cargo_runner_request,
        "correlation_id": correlation_id,
        "host_fingerprint": host_fingerprint,
        "budget_profile": budget_profile,
        "caveats": caveats,
    }


def resolve_cache_dir(target_dir: Path, raw_cache_dir: str | None) -> Path:
    if raw_cache_dir:
        cache_dir = Path(raw_cache_dir).expanduser()
        if cache_dir.is_absolute():
            return cache_dir
        return target_dir / cache_dir
    return target_dir / "perf/evidence_cache"


def parse_utc_timestamp(raw: Any) -> datetime | None:
    if not isinstance(raw, str) or not raw.strip():
        return None
    text = raw.strip()
    if text.endswith("Z"):
        text = f"{text[:-1]}+00:00"
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def entry_string(entry: dict[str, Any], key: str) -> str | None:
    value = entry.get(key)
    if not isinstance(value, str):
        return None
    value = value.strip()
    return value or None


def entry_float(entry: dict[str, Any], key: str) -> float | None:
    value = entry.get(key)
    if isinstance(value, int | float):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError:
            return None
    return None


def resolve_entry_path(raw: str, base: Path) -> Path:
    path = Path(raw).expanduser()
    if path.is_absolute():
        return path
    return base / path


def read_jsonl_schema(path: Path) -> str | None:
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


def artifact_schema(path: Path) -> str | None:
    if path.suffix == ".json":
        payload = read_json(path)
        value = payload.get("schema") if payload else None
        return value if isinstance(value, str) else None
    if path.suffix == ".jsonl":
        return read_jsonl_schema(path)
    return None


def load_evidence_cache_entries(cache_dir: Path) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    index_path = cache_dir / "index.json"
    status: dict[str, Any] = {
        "schema": EVIDENCE_CACHE_SCHEMA,
        "cache_dir": str(cache_dir),
        "index_path": str(index_path),
        "enabled": True,
        "index_exists": index_path.is_file(),
        "index_valid": False,
        "entry_count": 0,
    }
    if not index_path.is_file():
        status["detail"] = "cache index not found"
        return [], status

    payload = read_json(index_path)
    if payload is None:
        status["detail"] = "cache index is missing or invalid JSON"
        return [], status
    if payload.get("schema") != EVIDENCE_CACHE_SCHEMA:
        status["detail"] = "cache index schema mismatch"
        status["observed_schema"] = payload.get("schema")
        return [], status

    raw_entries = payload.get("entries")
    if not isinstance(raw_entries, list):
        status["detail"] = "cache index entries must be an array"
        return [], status

    entries = [entry for entry in raw_entries if isinstance(entry, dict)]
    status["index_valid"] = True
    status["entry_count"] = len(entries)
    return entries, status


def validate_evidence_cache_entry(
    entry: dict[str, Any],
    group: ArtifactGroup,
    context: EvidenceCacheContext,
    now: datetime,
) -> tuple[dict[str, Any] | None, dict[str, Any] | None]:
    contract_id = entry_string(entry, "contract_id")
    rejection_base = {
        "contract_id": contract_id,
        "expected_contract_id": group.contract_id,
        "cache_index_path": str(context.cache_dir / "index.json"),
    }
    if entry.get("schema") != EVIDENCE_CACHE_ENTRY_SCHEMA:
        return None, {
            **rejection_base,
            "reason": "entry_schema_mismatch",
            "observed_schema": entry.get("schema"),
        }
    if contract_id != group.contract_id:
        return None, {
            **rejection_base,
            "reason": "contract_mismatch",
        }

    artifact_path_raw = entry_string(entry, "artifact_path")
    cache_artifact_path_raw = entry_string(entry, "cache_artifact_path")
    resolved_artifact_path = (
        resolve_entry_path(artifact_path_raw, context.repo_root)
        if artifact_path_raw
        else None
    )
    resolved_cache_artifact_path = (
        resolve_entry_path(cache_artifact_path_raw, context.cache_dir)
        if cache_artifact_path_raw
        else None
    )
    evidence_path = resolved_cache_artifact_path or resolved_artifact_path
    if evidence_path is None:
        return None, {
            **rejection_base,
            "reason": "missing_artifact_path",
        }
    if not evidence_path.is_file():
        return None, {
            **rejection_base,
            "reason": "cache_artifact_missing",
            "cache_artifact_path": str(evidence_path),
        }

    git_commit = entry_string(entry, "git_commit")
    if git_commit != context.git_commit or git_commit == "unknown":
        return None, {
            **rejection_base,
            "reason": "git_commit_mismatch",
            "expected_git_commit": context.git_commit,
            "observed_git_commit": git_commit,
        }

    build_profile = entry_string(entry, "build_profile")
    if build_profile != context.build_profile:
        return None, {
            **rejection_base,
            "reason": "build_profile_mismatch",
            "expected_build_profile": context.build_profile,
            "observed_build_profile": build_profile,
        }

    run_id = entry_string(entry, "run_id")
    correlation_id = entry_string(entry, "correlation_id")
    if run_id is None or correlation_id is None:
        return None, {
            **rejection_base,
            "reason": "missing_lineage",
            "run_id_present": run_id is not None,
            "correlation_id_present": correlation_id is not None,
        }

    if entry_string(entry, "command") is None:
        return None, {
            **rejection_base,
            "reason": "missing_command",
        }
    if entry_string(entry, "toolchain") is None:
        return None, {
            **rejection_base,
            "reason": "missing_toolchain",
        }
    host_fingerprint = entry.get("host_fingerprint")
    if not isinstance(host_fingerprint, dict) or not host_fingerprint:
        return None, {
            **rejection_base,
            "reason": "missing_host_fingerprint",
        }
    if "artifact_schema" not in entry:
        return None, {
            **rejection_base,
            "reason": "missing_artifact_schema_field",
        }

    created_at = parse_utc_timestamp(entry.get("created_at"))
    if created_at is None:
        return None, {
            **rejection_base,
            "reason": "missing_or_invalid_created_at",
        }
    ttl_hours = entry_float(entry, "ttl_hours")
    if ttl_hours is None or ttl_hours <= 0.0:
        return None, {
            **rejection_base,
            "reason": "missing_or_invalid_ttl",
            "observed_ttl_hours": entry.get("ttl_hours"),
        }
    effective_ttl_hours = min(ttl_hours, context.max_ttl_hours)
    age_hours = (now - created_at).total_seconds() / 3600.0
    if age_hours > effective_ttl_hours:
        return None, {
            **rejection_base,
            "reason": "cache_entry_expired",
            "age_hours": age_hours,
            "ttl_hours": ttl_hours,
            "effective_ttl_hours": effective_ttl_hours,
        }
    expires_at = parse_utc_timestamp(entry.get("expires_at"))
    if expires_at is not None and now >= expires_at:
        return None, {
            **rejection_base,
            "reason": "cache_entry_expired_at",
            "expires_at": entry.get("expires_at"),
        }

    expected_sha = entry_string(entry, "sha256")
    actual_sha = sha256_file(evidence_path)
    if expected_sha is None or actual_sha != expected_sha:
        return None, {
            **rejection_base,
            "reason": "checksum_mismatch",
            "expected_sha256": expected_sha,
            "actual_sha256": actual_sha,
            "cache_artifact_path": str(evidence_path),
        }

    detected_schema = artifact_schema(evidence_path)
    recorded_schema = entry.get("artifact_schema")
    if detected_schema is not None and recorded_schema != detected_schema:
        return None, {
            **rejection_base,
            "reason": "artifact_schema_mismatch",
            "expected_artifact_schema": detected_schema,
            "observed_artifact_schema": recorded_schema,
        }

    accepted = {
        "source_kind": "cache",
        "path": str(evidence_path),
        "artifact_path": str(resolved_artifact_path) if resolved_artifact_path else None,
        "cache_artifact_path": str(evidence_path),
        "cache_index_path": str(context.cache_dir / "index.json"),
        "age_hours": age_hours,
        "max_age_hours": effective_ttl_hours,
        "size_bytes": evidence_path.stat().st_size,
        "sha256": actual_sha,
        "schema": EVIDENCE_CACHE_ENTRY_SCHEMA,
        "artifact_schema": recorded_schema,
        "git_commit": git_commit,
        "build_profile": build_profile,
        "run_id": run_id,
        "correlation_id": correlation_id,
        "command": entry_string(entry, "command"),
        "toolchain": entry_string(entry, "toolchain"),
        "host_fingerprint": host_fingerprint,
        "created_at": entry.get("created_at"),
        "ttl_hours": ttl_hours,
        "expires_at": entry.get("expires_at"),
        "reused_evidence": True,
    }
    return accepted, None


def evidence_cache_for_group(
    entries: list[dict[str, Any]],
    group: ArtifactGroup,
    context: EvidenceCacheContext,
    now: datetime,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    accepted: list[dict[str, Any]] = []
    rejected: list[dict[str, Any]] = []
    for entry in entries:
        if entry.get("contract_id") != group.contract_id:
            continue
        accepted_entry, rejected_entry = validate_evidence_cache_entry(entry, group, context, now)
        if accepted_entry is not None:
            accepted.append(accepted_entry)
        if rejected_entry is not None:
            rejected.append(rejected_entry)
    return accepted, rejected


def parse_budget_contracts(perf_budgets_rs: Path) -> list[BudgetContract]:
    text = perf_budgets_rs.read_text(encoding="utf-8")
    contracts: list[BudgetContract] = []
    for block in re.findall(r"Budget\s*\{(.*?)\},", text, flags=re.S):
        name = re.search(r'name:\s*"([^"]+)"', block)
        category = re.search(r'category:\s*"([^"]+)"', block)
        methodology = re.search(r'methodology:\s*"([^"]+)"', block)
        ci_enforced = re.search(r"ci_enforced:\s*(true|false)", block)
        if not name or not category or not methodology or not ci_enforced:
            continue
        contracts.append(
            BudgetContract(
                name=name.group(1),
                category=category.group(1),
                methodology=methodology.group(1),
                ci_enforced=ci_enforced.group(1) == "true",
            )
        )
    return contracts


def file_age_hours(path: Path, now: datetime) -> float | None:
    try:
        modified = datetime.fromtimestamp(path.stat().st_mtime, tz=timezone.utc)
    except OSError:
        return None
    return (now - modified).total_seconds() / 3600.0


def existing_fresh_candidates(
    candidates: tuple[Path, ...], max_age_hours: float, now: datetime
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    fresh: list[dict[str, Any]] = []
    stale: list[dict[str, Any]] = []
    for path in candidates:
        if not path.is_file():
            continue
        age = file_age_hours(path, now)
        artifact = {
            "source_kind": "direct",
            "path": str(path),
            "age_hours": age,
            "max_age_hours": max_age_hours,
            "size_bytes": path.stat().st_size,
            "sha256": sha256_file(path),
            "reused_evidence": False,
        }
        if age is not None and age <= max_age_hours:
            fresh.append(artifact)
        else:
            stale.append(artifact)
    return fresh, stale


def glob_estimates(base: Path) -> tuple[Path, ...]:
    pattern = str(base / "*" / "new" / "estimates.json")
    matches = tuple(Path(path) for path in sorted(glob.glob(pattern)))
    return matches or (base,)


def pijs_candidates_in_evidence_dir(evidence_dir: Path) -> tuple[Path, ...]:
    return tuple(
        evidence_dir / relative
        for relative in (
            "pijs_workload_perf.jsonl",
            "pijs_workload_release.jsonl",
            "pijs_workload_debug.jsonl",
            "pijs_workload.jsonl",
            "results/pijs_workload.jsonl",
            "perf/pijs_workload_perf.jsonl",
            "perf/pijs_workload_release.jsonl",
            "perf/pijs_workload_debug.jsonl",
            "perf/pijs_workload.jsonl",
            "perf/results/pijs_workload.jsonl",
        )
    )


def pijs_candidates(repo_root: Path, target_dir: Path) -> tuple[Path, ...]:
    paths: list[Path] = []
    for evidence_dir in perf_evidence_dirs(repo_root):
        paths.extend(pijs_candidates_in_evidence_dir(evidence_dir))
    perf_dir = target_dir / "perf"
    paths.extend(
        perf_dir / relative
        for relative in (
            "perf/pijs_workload_perf.jsonl",
            "release/pijs_workload_release.jsonl",
            "debug/pijs_workload_debug.jsonl",
            "pijs_workload.jsonl",
            "results/pijs_workload.jsonl",
        )
    )
    return dedupe_paths(paths)


def binary_candidates(
    repo_root: Path,
    target_dir: Path,
    release_override: str | None,
) -> tuple[Path, ...]:
    paths: list[Path] = []
    if release_override:
        paths.append(Path(release_override).expanduser())
    for evidence_dir in perf_evidence_dirs(repo_root):
        paths.extend((evidence_dir / "release" / "pi", evidence_dir / "perf" / "pi"))
    paths.extend((target_dir / "release" / "pi", target_dir / "perf" / "pi"))
    return dedupe_paths(paths)


CONTEXT_BUDGET_ARTIFACTS: tuple[tuple[str, str], ...] = (
    ("context_graph_build_cold_p95", "graph_build_cold"),
    ("context_graph_build_warm_p95", "graph_build_warm"),
    ("context_incremental_update_p95", "incremental_update"),
    ("context_planning_p95", "planning"),
    ("context_bundle_serialization_p95", "bundle_serialization"),
)
CONTEXT_BUDGET_JSON_NAMES: tuple[str, ...] = tuple(
    budget_name for budget_name, _bench_name in CONTEXT_BUDGET_ARTIFACTS
) + ("context_bundle_estimated_bytes_max",)


def context_criterion_relative(bench_name: str) -> Path:
    return Path("criterion/semantic_context") / bench_name / "large_workspace/new/estimates.json"


def context_artifact_groups(
    repo_root: Path,
    target_dir: Path,
    bench_prefix: str,
) -> list[ArtifactGroup]:
    suggested = (
        f"{bench_prefix} bench --bench semantic_context --profile perf semantic_context",
    )
    return [
        ArtifactGroup(
            contract_id=budget_name,
            budget_names=(budget_name,),
            candidates=evidence_then_target_paths(
                repo_root,
                target_dir,
                (str(context_criterion_relative(bench_name)),),
                (str(context_criterion_relative(bench_name)),),
            ),
            suggested_commands=suggested,
            reason=(
                f"semantic_context/{bench_name}/large_workspace Criterion estimate "
                "required by tests/perf_budgets.rs"
            ),
            expected_outputs=(
                target_dir / context_criterion_relative(bench_name),
                repo_root / "tests/perf/reports" / context_criterion_relative(bench_name),
            ),
        )
        for budget_name, bench_name in CONTEXT_BUDGET_ARTIFACTS
    ]


def context_budget_json_candidates(repo_root: Path, target_dir: Path) -> tuple[Path, ...]:
    paths: list[Path] = []
    explicit = os.environ.get("PERF_CONTEXT_INTELLIGENCE_BUDGET_JSON")
    if explicit:
        path = resolve_env_path(repo_root, explicit)
        if path is not None:
            paths.append(path)
    for evidence_dir in perf_evidence_dirs(repo_root):
        paths.extend(
            evidence_dir / relative
            for relative in (
                "context_intelligence_planner_budget.json",
                "results/context_intelligence_planner_budget.json",
                "perf/context_intelligence_planner_budget.json",
                "perf/results/context_intelligence_planner_budget.json",
                "context_intelligence/perf_budget.json",
                "perf/context_intelligence/perf_budget.json",
            )
        )
    paths.extend(
        target_dir / relative
        for relative in (
            "perf/context_intelligence_planner_budget.json",
            "perf/results/context_intelligence_planner_budget.json",
            "perf/context_intelligence/perf_budget.json",
        )
    )
    paths.append(repo_root / "tests/perf/reports/context_intelligence_planner_budget.json")
    return dedupe_paths(paths)


def artifact_groups(repo_root: Path, target_dir: Path) -> list[ArtifactGroup]:
    cargo_env = (
        'export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/pi_agent_rust_cargo/${USER:-agent}/target}" '
        'TMPDIR="${TMPDIR:-/data/tmp/pi_agent_rust_cargo/${USER:-agent}/tmp}" && '
        'mkdir -p "$CARGO_TARGET_DIR" "$TMPDIR"'
    )
    bench_prefix = f"{cargo_env} && rch exec -- cargo"
    evidence_env = (
        'PERF_EVIDENCE_DIR="${PERF_EVIDENCE_DIR:-tests/perf/reports}" '
        "rch exec -- cargo"
    )
    return [
        ArtifactGroup(
            contract_id="startup_version_p95",
            budget_names=("startup_version_p95",),
            candidates=evidence_then_target_paths(
                repo_root,
                target_dir,
                ("criterion/startup/version/warm/new/estimates.json",),
                ("criterion/startup/version/warm/new/estimates.json",),
            ),
            suggested_commands=(
                f"{bench_prefix} bench --bench system --profile perf startup",
            ),
            reason="startup/version Criterion estimate required by tests/perf_budgets.rs",
            expected_outputs=(
                target_dir / "criterion/startup/version/warm/new/estimates.json",
                repo_root / "tests/perf/reports/criterion/startup/version/warm/new/estimates.json",
            ),
        ),
        ArtifactGroup(
            contract_id="extension_criterion_load_init",
            budget_names=("ext_cold_load_simple_p95",),
            candidates=evidence_then_target_paths(
                repo_root,
                target_dir,
                ("criterion/ext_load_init/load_init_cold/hello/new/estimates.json",),
                ("criterion/ext_load_init/load_init_cold/hello/new/estimates.json",),
            ),
            suggested_commands=(
                f"{bench_prefix} bench --bench extension_budget_inputs --profile perf ext_load_init",
            ),
            reason="ext_load_init/load_init_cold/hello Criterion estimate required by tests/perf_budgets.rs",
            expected_outputs=(
                target_dir
                / "criterion/ext_load_init/load_init_cold/hello/new/estimates.json",
                repo_root
                / "tests/perf/reports/criterion/ext_load_init/load_init_cold/hello/new/estimates.json",
            ),
            blocker=EXTENSION_BLOCKER_BEAD,
        ),
        *context_artifact_groups(repo_root, target_dir, bench_prefix),
        ArtifactGroup(
            contract_id="context_intelligence_budget_artifact",
            budget_names=CONTEXT_BUDGET_JSON_NAMES,
            candidates=context_budget_json_candidates(repo_root, target_dir),
            suggested_commands=(
                f"{bench_prefix} bench --bench semantic_context --profile perf semantic_context",
            ),
            reason="context_intelligence_planner_budget.json required by tests/perf_budgets.rs",
            expected_outputs=(
                target_dir / "perf/context_intelligence/perf_budget.json",
                repo_root / "tests/perf/reports/context_intelligence_planner_budget.json",
            ),
        ),
        ArtifactGroup(
            contract_id="pijs_workload",
            budget_names=("tool_call_latency_mean", "tool_call_throughput_min"),
            candidates=pijs_candidates(repo_root, target_dir),
            suggested_commands=(
                f"{cargo_env} && BENCH_CARGO_RUNNER=rch BENCH_ALLOCATORS_CSV=system BENCH_PGO_MODE=off ITERATIONS=2000 TOOL_CALLS_CSV=1,10 ./scripts/bench_extension_workloads.sh",
            ),
            reason="pijs_workload JSONL required for tool-call latency and throughput budgets",
            expected_outputs=pijs_candidates(repo_root, target_dir),
        ),
        ArtifactGroup(
            contract_id="extension_criterion_policy",
            budget_names=("policy_eval_p99",),
            candidates=dedupe_paths(
                [
                    candidate
                    for base in evidence_then_target_paths(
                        repo_root,
                        target_dir,
                        ("criterion/ext_policy/evaluate",),
                        ("criterion/ext_policy/evaluate",),
                    )
                    for candidate in glob_estimates(base)
                ]
            ),
            suggested_commands=(
                f"{bench_prefix} bench --bench extension_budget_inputs --profile perf ext_policy",
            ),
            reason="ext_policy/evaluate Criterion estimates required by tests/perf_budgets.rs",
            expected_outputs=(
                target_dir / "criterion/ext_policy/evaluate/*/new/estimates.json",
                repo_root / "tests/perf/reports/criterion/ext_policy/evaluate/*/new/estimates.json",
            ),
            blocker=EXTENSION_BLOCKER_BEAD,
        ),
        ArtifactGroup(
            contract_id="release_binary",
            budget_names=("binary_size_release",),
            candidates=binary_candidates(
                repo_root,
                target_dir,
                os.environ.get("PERF_RELEASE_BINARY_PATH"),
            ),
            suggested_commands=(
                f"{bench_prefix} build --bin pi --release",
            ),
            reason="release pi binary required for binary_size_release budget",
            expected_outputs=(target_dir / "release/pi", repo_root / "tests/perf/reports/release/pi"),
        ),
        ArtifactGroup(
            contract_id="extension_criterion_protocol",
            budget_names=("protocol_parse_p99",),
            candidates=dedupe_paths(
                [
                    candidate
                    for base in evidence_then_target_paths(
                        repo_root,
                        target_dir,
                        ("criterion/ext_protocol/parse_and_validate",),
                        ("criterion/ext_protocol/parse_and_validate",),
                    )
                    for candidate in glob_estimates(base)
                ]
            ),
            suggested_commands=(
                f"{bench_prefix} bench --bench extension_budget_inputs --profile perf ext_protocol",
            ),
            reason="ext_protocol/parse_and_validate Criterion estimates required by tests/perf_budgets.rs",
            expected_outputs=(
                target_dir / "criterion/ext_protocol/parse_and_validate/*/new/estimates.json",
                repo_root
                / "tests/perf/reports/criterion/ext_protocol/parse_and_validate/*/new/estimates.json",
            ),
            blocker=EXTENSION_BLOCKER_BEAD,
        ),
        ArtifactGroup(
            contract_id="extension_benchmark_stratification",
            budget_names=(),
            candidates=dedupe_paths(
                list(
                    evidence_then_target_paths(
                        repo_root,
                        target_dir,
                        (
                            "extension_benchmark_stratification.json",
                            "perf/extension_benchmark_stratification.json",
                            "results/extension_benchmark_stratification.json",
                            "perf/results/extension_benchmark_stratification.json",
                        ),
                        (
                            "perf/extension_benchmark_stratification.json",
                            "perf/results/extension_benchmark_stratification.json",
                        ),
                    )
                )
                + [
                repo_root / "tests/perf/reports/extension_benchmark_stratification.json",
                ]
            ),
            suggested_commands=(
                f"PI_GENERATE_PERF_BUDGET_REPORT=1 {evidence_env} test --test perf_budgets --profile perf generate_budget_report -- --nocapture",
            ),
            reason="global extension claim data contract consumed by collect_data_contract_failures",
            expected_outputs=(
                target_dir / "perf/extension_benchmark_stratification.json",
                repo_root / "tests/perf/reports/extension_benchmark_stratification.json",
            ),
        ),
        ArtifactGroup(
            contract_id="phase1_matrix_validation",
            budget_names=(),
            candidates=dedupe_paths(
                list(
                    evidence_then_target_paths(
                        repo_root,
                        target_dir,
                        (
                            "phase1_matrix_validation.json",
                            "results/phase1_matrix_validation.json",
                            "perf/results/phase1_matrix_validation.json",
                        ),
                        ("perf/results/phase1_matrix_validation.json",),
                    )
                )
                + [
                repo_root / "tests/perf/reports/phase1_matrix_validation.json",
                ]
            ),
            suggested_commands=(
                f"PI_GENERATE_PERF_BUDGET_REPORT=1 {evidence_env} test --test perf_budgets --profile perf generate_budget_report -- --nocapture",
            ),
            reason="phase1 weighted attribution data contract consumed by collect_data_contract_failures",
            expected_outputs=(
                target_dir / "perf/results/phase1_matrix_validation.json",
                repo_root / "tests/perf/reports/phase1_matrix_validation.json",
            ),
        ),
    ]


def read_json(path: Path) -> dict[str, Any] | None:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return payload if isinstance(payload, dict) else None


def report_status(repo_root: Path, now: datetime) -> dict[str, Any]:
    path = repo_root / "tests/perf/reports/budget_summary.json"
    payload = read_json(path)
    base: dict[str, Any] = {
        "path": str(path),
        "exists": path.exists(),
        "age_hours": file_age_hours(path, now) if path.exists() else None,
        "schema": payload.get("schema") if payload else None,
        "generated_at": payload.get("generated_at") if payload else None,
    }
    if payload:
        for key in (
            "fail",
            "no_data",
            "ci_fail",
            "ci_no_data",
            "data_contract_failures_count",
        ):
            base[key] = payload.get(key)
    return base


def rch_status(skip: bool) -> dict[str, Any]:
    path = shutil.which("rch")
    if path is None:
        return {
            "available": False,
            "healthy": False,
            "checked": False,
            "command": "rch check --quiet",
            "detail": "rch executable not found in PATH",
        }
    if skip:
        return {
            "available": True,
            "healthy": None,
            "checked": False,
            "command": "rch check --quiet",
            "detail": "skipped by --skip-rch-check",
        }
    try:
        result = subprocess.run(
            [path, "check", "--quiet"],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=10,
        )
    except subprocess.TimeoutExpired:
        return {
            "available": True,
            "healthy": False,
            "checked": True,
            "command": "rch check --quiet",
            "detail": "timed out after 10s",
        }
    detail = (result.stderr or result.stdout or "").strip()
    return {
        "available": True,
        "healthy": result.returncode == 0,
        "checked": True,
        "command": "rch check --quiet",
        "detail": detail,
    }


def build_report(args: argparse.Namespace) -> tuple[int, dict[str, Any]]:
    repo_root = Path(args.repo_root).resolve() if args.repo_root else repo_root_from_script()
    now = utc_now()
    target_dir = resolve_target_dir(repo_root, args.cargo_target_dir or os.environ.get("CARGO_TARGET_DIR"))
    max_age_hours = args.max_age_hours
    cache_dir = resolve_cache_dir(
        target_dir,
        args.evidence_cache_dir or os.environ.get("PI_PERF_EVIDENCE_CACHE_DIR"),
    )
    cache_context = EvidenceCacheContext(
        repo_root=repo_root,
        target_dir=target_dir,
        cache_dir=cache_dir,
        git_commit=args.cache_git_commit
        or os.environ.get("PI_PERF_GIT_COMMIT")
        or current_git_commit(repo_root),
        build_profile=args.cache_profile
        or os.environ.get("PERF_PROFILE")
        or os.environ.get("CARGO_PROFILE")
        or "perf",
        max_ttl_hours=args.cache_ttl_hours,
    )
    cache_entries, cache_status = load_evidence_cache_entries(cache_dir)
    contracts = parse_budget_contracts(repo_root / "tests/perf_budgets.rs")
    ci_contracts = [contract for contract in contracts if contract.ci_enforced]

    missing: list[dict[str, Any]] = []
    stale: list[dict[str, Any]] = []
    fresh: list[dict[str, Any]] = []
    rejected_cache_entries: list[dict[str, Any]] = []
    suggestions: list[str] = []
    expected_outputs: list[str] = []
    recognized_blockers: list[dict[str, Any]] = []

    groups = artifact_groups(repo_root, target_dir)
    for group in groups:
        group_fresh, group_stale = existing_fresh_candidates(group.candidates, max_age_hours, now)
        cache_fresh, cache_rejected = evidence_cache_for_group(
            cache_entries,
            group,
            cache_context,
            now,
        )
        rejected_cache_entries.extend(cache_rejected)
        expected_outputs.extend(str(path) for path in group.expected_outputs)
        if group_fresh:
            fresh.append(
                {
                    "contract_id": group.contract_id,
                    "budget_names": list(group.budget_names),
                    "evidence_source": "direct",
                    "artifacts": group_fresh,
                }
            )
            continue
        if cache_fresh:
            fresh.append(
                {
                    "contract_id": group.contract_id,
                    "budget_names": list(group.budget_names),
                    "evidence_source": "cache",
                    "artifacts": cache_fresh,
                }
            )
            continue
        issue = {
            "contract_id": group.contract_id,
            "budget_names": list(group.budget_names),
            "reason": group.reason,
            "expected_paths": [str(path) for path in group.candidates],
            "suggested_commands": list(group.suggested_commands),
            "blocker": group.blocker,
        }
        missing.append(issue)
        suggestions.extend(group.suggested_commands)
        if group_stale:
            for artifact in group_stale:
                artifact["contract_id"] = group.contract_id
                artifact["budget_names"] = list(group.budget_names)
                stale.append(artifact)
        if group.blocker:
            recognized_blockers.append(
                {
                    "bead": group.blocker,
                    "contract_id": group.contract_id,
                    "budget_names": list(group.budget_names),
                    "detail": "missing or stale extension Criterion input for bd-2zcs5.51",
                }
            )

    report = report_status(repo_root, now)
    report_blockers: list[str] = []
    for key in (
        "fail",
        "no_data",
        "ci_fail",
        "ci_no_data",
        "data_contract_failures_count",
    ):
        value = report.get(key)
        if isinstance(value, int | float) and value != 0:
            report_blockers.append(f"budget_summary.{key}={value}")

    dedup_suggestions = list(dict.fromkeys(suggestions))
    dedup_expected = list(dict.fromkeys(expected_outputs))
    ready = not missing and not stale and not report_blockers
    cache_status.update(
        {
            "expected_git_commit": cache_context.git_commit,
            "expected_build_profile": cache_context.build_profile,
            "max_ttl_hours": cache_context.max_ttl_hours,
            "accepted_entry_count": sum(
                1
                for item in fresh
                if item.get("evidence_source") == "cache"
                for _artifact in item.get("artifacts", [])
            ),
            "rejected_entry_count": len(rejected_cache_entries),
        }
    )
    payload: dict[str, Any] = {
        "schema": SCHEMA,
        "generated_at": iso_now(),
        "repo_root": str(repo_root),
        "budget_contract_source": {
            "path": str(repo_root / "tests/perf_budgets.rs"),
            "sha256": sha256_file(repo_root / "tests/perf_budgets.rs"),
            "total_budgets": len(contracts),
            "ci_enforced_budgets": [contract.name for contract in ci_contracts],
        },
        "cargo_target_dir": str(target_dir),
        "max_artifact_age_hours": max_age_hours,
        "evidence_cache": cache_status,
        "rch": rch_status(args.skip_rch_check),
        "current_report": report,
        "readiness": "ready" if ready else "blocked",
        "missing_budget_artifacts": missing,
        "stale_artifacts": stale,
        "fresh_artifacts": fresh,
        "rejected_evidence_cache_entries": rejected_cache_entries,
        "recognized_blockers": recognized_blockers,
        "suggested_commands": dedup_suggestions,
        "expected_output_paths": dedup_expected,
        "safety_notes": [
            "All CPU-intensive cargo refresh commands must be run through rch exec -- ...",
            "Set CARGO_TARGET_DIR and TMPDIR to /data/tmp/pi_agent_rust_cargo/${USER:-agent}/... before refreshing evidence.",
            "For RCH report generation, stage required artifacts into a repo-visible evidence root and set PERF_EVIDENCE_DIR plus PI_GENERATE_PERF_BUDGET_REPORT=1 for cargo test --test perf_budgets generate_budget_report.",
            "Cached perf evidence is reusable only when commit, build profile, TTL, lineage, schema, and checksum validation pass; reused entries are labeled source_kind=cache.",
            "Do not refresh tests/perf/reports/budget_summary.json until missing_budget_artifacts and stale_artifacts are empty.",
        ],
        "report_blockers": report_blockers,
    }
    return (0 if ready else 1), payload


def write_json(payload: dict[str, Any]) -> None:
    print(json.dumps(payload, indent=2, sort_keys=True))


def run_self_test() -> int:
    def write_host_fixture(
        root: Path,
        *,
        cpu_count: int = 8,
        mem_kb: int = 16 * 1024 * 1024,
        cgroup_rel: str = "agent.slice/pi.scope",
        cpu_max: str = "max 100000",
        cpuset: str | None = "0-7",
        memory_max: str = "max",
        numa_nodes: int = 2,
    ) -> tuple[Path, Path]:
        proc_root = root / "proc"
        sys_root = root / "sys"
        (proc_root / "self").mkdir(parents=True)
        (proc_root / "self/cgroup").write_text(f"0::/{cgroup_rel}\n", encoding="utf-8")
        cpuinfo = []
        for index in range(cpu_count):
            cpuinfo.append(f"processor\t: {index}\nmodel name\t: Fixture CPU\n")
        (proc_root / "cpuinfo").write_text("\n".join(cpuinfo), encoding="utf-8")
        (proc_root / "meminfo").write_text(f"MemTotal:       {mem_kb} kB\n", encoding="utf-8")

        cgroup_dir = sys_root / "fs/cgroup" / cgroup_rel
        cgroup_dir.mkdir(parents=True)
        (cgroup_dir / "cpu.max").write_text(f"{cpu_max}\n", encoding="utf-8")
        if cpuset is not None:
            (cgroup_dir / "cpuset.cpus.effective").write_text(f"{cpuset}\n", encoding="utf-8")
        (cgroup_dir / "memory.max").write_text(f"{memory_max}\n", encoding="utf-8")
        node_root = sys_root / "devices/system/node"
        for index in range(numa_nodes):
            (node_root / f"node{index}").mkdir(parents=True)
        return proc_root, sys_root

    def build_args(
        root: Path,
        *,
        cache_dir: Path | None = None,
        cache_git_commit: str = "test-commit",
        cache_profile: str = "perf",
        cache_ttl_hours: float = 24.0,
    ) -> argparse.Namespace:
        return argparse.Namespace(
            repo_root=str(root),
            cargo_target_dir=str(root / "target"),
            max_age_hours=24.0,
            evidence_cache_dir=str(cache_dir) if cache_dir else None,
            cache_ttl_hours=cache_ttl_hours,
            cache_profile=cache_profile,
            cache_git_commit=cache_git_commit,
            skip_rch_check=True,
        )

    def write_cache_index(
        root: Path,
        *,
        contract_id: str = "extension_criterion_policy",
        cache_git_commit: str = "test-commit",
        cache_profile: str = "perf",
        created_at: str | None = None,
        ttl_hours: float = 24.0,
        run_id: str | None = "run-123",
        correlation_id: str | None = "corr-123",
    ) -> Path:
        cache_dir = root / "target/perf/evidence_cache"
        artifact_path = cache_dir / "artifacts" / contract_id / "estimates.json"
        artifact_path.parent.mkdir(parents=True, exist_ok=True)
        artifact_path.write_text('{"mean":{"point_estimate":1000.0}}\n', encoding="utf-8")
        entry = {
            "schema": EVIDENCE_CACHE_ENTRY_SCHEMA,
            "contract_id": contract_id,
            "artifact_path": str(root / "target/criterion/ext_policy/evaluate/safe/new/estimates.json"),
            "cache_artifact_path": str(artifact_path),
            "command": "rch exec -- cargo bench --bench extension_budget_inputs --profile perf ext_policy",
            "git_commit": cache_git_commit,
            "toolchain": "rustc 1.85.0-test",
            "host_fingerprint": {"hostname": "self-test", "cpu_count": 1},
            "build_profile": cache_profile,
            "run_id": run_id,
            "correlation_id": correlation_id,
            "sha256": sha256_file(artifact_path),
            "artifact_schema": None,
            "created_at": created_at or iso_now(),
            "ttl_hours": ttl_hours,
        }
        (cache_dir / "index.json").write_text(
            json.dumps(
                {
                    "schema": EVIDENCE_CACHE_SCHEMA,
                    "generated_at": iso_now(),
                    "entries": [entry],
                }
            ),
            encoding="utf-8",
        )
        return cache_dir

    def write_fixture(root: Path, include_policy: bool) -> None:
        (root / "tests/perf/reports").mkdir(parents=True)
        (root / "target/criterion/ext_load_init/load_init_cold/hello/new").mkdir(parents=True)
        if include_policy:
            (root / "target/criterion/ext_policy/evaluate/safe/new").mkdir(parents=True)
        (root / "target/criterion/ext_protocol/parse_and_validate/log/new").mkdir(parents=True)
        (root / "target/perf/perf").mkdir(parents=True)
        (root / "target/release").mkdir(parents=True)
        (root / "target/perf/results").mkdir(parents=True)
        (root / "tests/perf_budgets.rs").write_text(
            """
            const BUDGETS: &[Budget] = &[
              Budget { name: "startup_version_p95", category: "startup", metric: "p95", unit: "ms", threshold: 100.0, methodology: "criterion: startup", ci_enforced: true },
              Budget { name: "ext_cold_load_simple_p95", category: "extension", metric: "p95", unit: "ms", threshold: 5.0, methodology: "criterion: ext_load_init", ci_enforced: true },
              Budget { name: "tool_call_latency_mean", category: "tool_call", metric: "mean", unit: "us", threshold: 200.0, methodology: "pijs_workload", ci_enforced: true },
              Budget { name: "tool_call_throughput_min", category: "tool_call", metric: "min", unit: "calls/sec", threshold: 5000.0, methodology: "pijs_workload", ci_enforced: true },
              Budget { name: "context_graph_build_cold_p95", category: "context_intelligence", metric: "p95", unit: "ms", threshold: 500.0, methodology: "criterion: semantic_context/graph_build_cold", ci_enforced: true },
              Budget { name: "context_graph_build_warm_p95", category: "context_intelligence", metric: "p95", unit: "ms", threshold: 250.0, methodology: "criterion: semantic_context/graph_build_warm", ci_enforced: true },
              Budget { name: "context_incremental_update_p95", category: "context_intelligence", metric: "p95", unit: "ms", threshold: 250.0, methodology: "criterion: semantic_context/incremental_update", ci_enforced: true },
              Budget { name: "context_planning_p95", category: "context_intelligence", metric: "p95", unit: "ms", threshold: 50.0, methodology: "criterion: semantic_context/planning", ci_enforced: true },
              Budget { name: "context_bundle_serialization_p95", category: "context_intelligence", metric: "p95", unit: "ms", threshold: 25.0, methodology: "criterion: semantic_context/bundle_serialization", ci_enforced: true },
              Budget { name: "context_bundle_estimated_bytes_max", category: "context_intelligence", metric: "bytes", unit: "bytes", threshold: 262144.0, methodology: "semantic_context budget artifact", ci_enforced: true },
              Budget { name: "policy_eval_p99", category: "policy", metric: "p99", unit: "ns", threshold: 500.0, methodology: "criterion: ext_policy", ci_enforced: true },
              Budget { name: "idle_memory_rss", category: "memory", metric: "RSS", unit: "MB", threshold: 50.0, methodology: "sysinfo", ci_enforced: true },
              Budget { name: "binary_size_release", category: "binary", metric: "size", unit: "MB", threshold: BINARY_SIZE_RELEASE_BUDGET_MB, methodology: "ls", ci_enforced: true },
              Budget { name: "protocol_parse_p99", category: "protocol", metric: "p99", unit: "us", threshold: 50.0, methodology: "criterion: ext_protocol", ci_enforced: true },
            ];
            """,
            encoding="utf-8",
        )
        fresh_payload = {"mean": {"point_estimate": 1000.0}}
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
            path.write_text(json.dumps(fresh_payload), encoding="utf-8")
        (root / "target/perf/perf/pijs_workload_perf.jsonl").write_text(
            '{"schema":"pi.perf.workload.v1","iterations":2000,"tool_calls_per_iteration":1,"total_calls":2000,"build_profile_verified":true}\n',
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
        (root / "tests/perf/reports/budget_summary.json").write_text(
            json.dumps(
                {
                    "schema": "pi.perf.budget_summary.v2",
                    "generated_at": iso_now(),
                    "source_commit": "1" * 40,
                    "run_id": "preflight-self-test",
                    "correlation_id": "preflight-self-test",
                    "strict_mode": True,
                    "total_budgets": 0,
                    "ci_enforced": 0,
                    "ci_with_data": 0,
                    "ci_fail": 0,
                    "ci_no_data": 0,
                    "pass": 0,
                    "fail": 0,
                    "no_data": 0,
                    "data_contract_failures_count": 0,
                    "failing_data_contracts": [],
                    "budgets": [],
                    "budget_results": [],
                    "claim_readiness": {
                        "status": "claim_ready",
                        "performance_claims_authorized": True,
                        "blocking_reason_codes": [],
                    },
                }
            ),
            encoding="utf-8",
        )

    ok_root = Path(tempfile.mkdtemp(prefix="pi-perf-preflight-ok-"))
    write_fixture(ok_root, include_policy=True)
    ok_code, ok_payload = build_report(build_args(ok_root))
    assert ok_code == 0, ok_payload
    assert ok_payload["readiness"] == "ready", ok_payload

    aggregate_blocked_root = Path(
        tempfile.mkdtemp(prefix="pi-perf-preflight-aggregate-blocked-")
    )
    write_fixture(aggregate_blocked_root, include_policy=True)
    aggregate_summary_path = (
        aggregate_blocked_root / "tests/perf/reports/budget_summary.json"
    )
    aggregate_summary = json.loads(aggregate_summary_path.read_text(encoding="utf-8"))
    aggregate_summary["fail"] = 1
    aggregate_summary["no_data"] = 1
    aggregate_summary["claim_readiness"] = {
        "status": "blocked",
        "performance_claims_authorized": False,
        "blocking_reason_codes": ["budget_data_missing", "budget_failed"],
    }
    aggregate_summary_path.write_text(
        json.dumps(aggregate_summary),
        encoding="utf-8",
    )
    aggregate_code, aggregate_payload = build_report(
        build_args(aggregate_blocked_root)
    )
    assert aggregate_code == 1, aggregate_payload
    assert aggregate_payload["readiness"] == "blocked", aggregate_payload
    assert "budget_summary.fail=1" in aggregate_payload["report_blockers"], (
        aggregate_payload
    )
    assert "budget_summary.no_data=1" in aggregate_payload["report_blockers"], (
        aggregate_payload
    )

    blocked_root = Path(tempfile.mkdtemp(prefix="pi-perf-preflight-blocked-"))
    write_fixture(blocked_root, include_policy=False)
    blocked_code, blocked_payload = build_report(build_args(blocked_root))
    assert blocked_code == 1, blocked_payload
    assert blocked_payload["readiness"] == "blocked", blocked_payload
    assert any(
        item["contract_id"] == "extension_criterion_policy"
        for item in blocked_payload["missing_budget_artifacts"]
    ), blocked_payload
    assert any(
        item["bead"] == EXTENSION_BLOCKER_BEAD
        for item in blocked_payload["recognized_blockers"]
    ), blocked_payload
    extension_commands = [
        command
        for item in blocked_payload["missing_budget_artifacts"]
        if item["contract_id"].startswith("extension_criterion_")
        for command in item["suggested_commands"]
    ]
    assert extension_commands, blocked_payload
    assert all("--bench extension_budget_inputs" in command for command in extension_commands), (
        blocked_payload,
        extension_commands,
    )
    assert not any("--bench extensions" in command for command in extension_commands), (
        blocked_payload,
        extension_commands,
    )

    cached_root = Path(tempfile.mkdtemp(prefix="pi-perf-preflight-cache-ok-"))
    write_fixture(cached_root, include_policy=False)
    cache_dir = write_cache_index(cached_root)
    cached_code, cached_payload = build_report(build_args(cached_root, cache_dir=cache_dir))
    assert cached_code == 0, cached_payload
    assert cached_payload["readiness"] == "ready", cached_payload
    assert cached_payload["evidence_cache"]["accepted_entry_count"] == 1, cached_payload
    assert any(
        item["contract_id"] == "extension_criterion_policy"
        and item["evidence_source"] == "cache"
        for item in cached_payload["fresh_artifacts"]
    ), cached_payload

    stale_cache_root = Path(tempfile.mkdtemp(prefix="pi-perf-preflight-cache-stale-"))
    write_fixture(stale_cache_root, include_policy=False)
    stale_cache_dir = write_cache_index(
        stale_cache_root,
        created_at="2000-01-01T00:00:00Z",
        ttl_hours=1.0,
    )
    stale_code, stale_payload = build_report(
        build_args(stale_cache_root, cache_dir=stale_cache_dir)
    )
    assert stale_code == 1, stale_payload
    assert any(
        entry["reason"] == "cache_entry_expired"
        for entry in stale_payload["rejected_evidence_cache_entries"]
    ), stale_payload

    wrong_commit_root = Path(tempfile.mkdtemp(prefix="pi-perf-preflight-cache-commit-"))
    write_fixture(wrong_commit_root, include_policy=False)
    wrong_commit_cache_dir = write_cache_index(
        wrong_commit_root,
        cache_git_commit="other-commit",
    )
    wrong_commit_code, wrong_commit_payload = build_report(
        build_args(wrong_commit_root, cache_dir=wrong_commit_cache_dir)
    )
    assert wrong_commit_code == 1, wrong_commit_payload
    assert any(
        entry["reason"] == "git_commit_mismatch"
        for entry in wrong_commit_payload["rejected_evidence_cache_entries"]
    ), wrong_commit_payload

    wrong_profile_root = Path(tempfile.mkdtemp(prefix="pi-perf-preflight-cache-profile-"))
    write_fixture(wrong_profile_root, include_policy=False)
    wrong_profile_cache_dir = write_cache_index(
        wrong_profile_root,
        cache_profile="release",
    )
    wrong_profile_code, wrong_profile_payload = build_report(
        build_args(wrong_profile_root, cache_dir=wrong_profile_cache_dir)
    )
    assert wrong_profile_code == 1, wrong_profile_payload
    assert any(
        entry["reason"] == "build_profile_mismatch"
        for entry in wrong_profile_payload["rejected_evidence_cache_entries"]
    ), wrong_profile_payload

    missing_lineage_root = Path(tempfile.mkdtemp(prefix="pi-perf-preflight-cache-lineage-"))
    write_fixture(missing_lineage_root, include_policy=False)
    missing_lineage_cache_dir = write_cache_index(
        missing_lineage_root,
        run_id=None,
        correlation_id="",
    )
    missing_lineage_code, missing_lineage_payload = build_report(
        build_args(missing_lineage_root, cache_dir=missing_lineage_cache_dir)
    )
    assert missing_lineage_code == 1, missing_lineage_payload
    assert any(
        entry["reason"] == "missing_lineage"
        for entry in missing_lineage_payload["rejected_evidence_cache_entries"]
    ), missing_lineage_payload

    bare_root = Path(tempfile.mkdtemp(prefix="pi-perf-host-bare-"))
    bare_proc, bare_sys = write_host_fixture(bare_root)
    bare_fingerprint = build_host_topology_fingerprint(
        proc_root=bare_proc,
        sys_root=bare_sys,
        timestamp="2026-05-09T00:00:00Z",
        build_profile="perf",
    )
    assert bare_fingerprint["schema"] == "pi.perf.env_fingerprint.v1", bare_fingerprint
    assert bare_fingerprint["host_topology_schema"] == HOST_TOPOLOGY_SCHEMA, bare_fingerprint
    assert bare_fingerprint["cpu_cores"] == 8, bare_fingerprint
    assert bare_fingerprint["mem_total_mb"] == 16 * 1024, bare_fingerprint
    assert bare_fingerprint["host_fingerprint"]["numa"]["node_count"] == 2, bare_fingerprint

    quota_root = Path(tempfile.mkdtemp(prefix="pi-perf-host-quota-"))
    quota_proc, quota_sys = write_host_fixture(
        quota_root,
        cpu_max="200000 100000",
        cpuset="0-7",
        memory_max=str(2 * 1024 * 1024 * 1024),
    )
    quota_fingerprint = build_host_topology_fingerprint(
        proc_root=quota_proc,
        sys_root=quota_sys,
        timestamp="2026-05-09T00:00:00Z",
    )
    assert quota_fingerprint["cpu_cores"] == 2, quota_fingerprint
    assert quota_fingerprint["mem_total_mb"] == 2048, quota_fingerprint
    assert quota_fingerprint["budget_profile"]["target_cpu_cores"] == 2, quota_fingerprint
    assert "container_cpu_constraint_below_host" in quota_fingerprint["caveats"], quota_fingerprint
    assert "container_memory_constraint_below_host" in quota_fingerprint["caveats"], quota_fingerprint

    cpuset_root = Path(tempfile.mkdtemp(prefix="pi-perf-host-cpuset-"))
    cpuset_proc, cpuset_sys = write_host_fixture(
        cpuset_root,
        cpu_max="max 100000",
        cpuset="1,3-4",
    )
    cpuset_fingerprint = build_host_topology_fingerprint(
        proc_root=cpuset_proc,
        sys_root=cpuset_sys,
        timestamp="2026-05-09T00:00:00Z",
    )
    assert cpuset_fingerprint["cpu_cores"] == 3, cpuset_fingerprint
    assert cpuset_fingerprint["host_fingerprint"]["cgroup"]["cpuset_cpu_count"] == 3, cpuset_fingerprint

    missing_proc_root = Path(tempfile.mkdtemp(prefix="pi-perf-host-missing-proc-"))
    missing_fingerprint = build_host_topology_fingerprint(
        proc_root=missing_proc_root / "missing-proc",
        sys_root=missing_proc_root / "missing-sys",
        timestamp="2026-05-09T00:00:00Z",
    )
    assert "missing_procfs" in missing_fingerprint["caveats"], missing_fingerprint
    assert missing_fingerprint["cpu_cores"] >= 1, missing_fingerprint

    malformed_root = Path(tempfile.mkdtemp(prefix="pi-perf-host-malformed-"))
    malformed_proc, malformed_sys = write_host_fixture(
        malformed_root,
        cpu_max="not-a-quota",
        cpuset="x-y",
        memory_max="nope",
    )
    malformed_fingerprint = build_host_topology_fingerprint(
        proc_root=malformed_proc,
        sys_root=malformed_sys,
        timestamp="2026-05-09T00:00:00Z",
    )
    assert "malformed_cpu_max" in malformed_fingerprint["caveats"], malformed_fingerprint
    assert "malformed_cpuset" in malformed_fingerprint["caveats"], malformed_fingerprint
    assert "malformed_memory_max" in malformed_fingerprint["caveats"], malformed_fingerprint
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo-root", help="Repository root. Defaults to this script's repo.")
    parser.add_argument(
        "--cargo-target-dir",
        help="Cargo target directory to inspect. Defaults to CARGO_TARGET_DIR or ./target.",
    )
    parser.add_argument(
        "--max-age-hours",
        type=float,
        default=float(os.environ.get("PI_PERF_MAX_ARTIFACT_AGE_HOURS", DEFAULT_MAX_ARTIFACT_AGE_HOURS)),
        help="Maximum accepted artifact age in hours.",
    )
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
        help="Maximum reusable cache TTL in hours; entry ttl_hours is capped by this value.",
    )
    parser.add_argument(
        "--cache-profile",
        help="Expected cached evidence build profile. Defaults to PERF_PROFILE, CARGO_PROFILE, or perf.",
    )
    parser.add_argument(
        "--cache-git-commit",
        help="Expected cached evidence git commit. Defaults to PI_PERF_GIT_COMMIT or current HEAD.",
    )
    parser.add_argument(
        "--skip-rch-check",
        action="store_true",
        help="Do not run rch check --quiet; useful in hermetic self-tests.",
    )
    parser.add_argument(
        "--host-fingerprint",
        action="store_true",
        help="Emit cgroup-aware host topology fingerprint JSON and exit.",
    )
    parser.add_argument("--proc-root", default="/proc", help="procfs root for --host-fingerprint.")
    parser.add_argument("--sys-root", default="/sys", help="sysfs root for --host-fingerprint.")
    parser.add_argument("--fingerprint-timestamp", help="Timestamp recorded in host fingerprint.")
    parser.add_argument("--fingerprint-build-profile", help="Build profile recorded in host fingerprint.")
    parser.add_argument("--fingerprint-pgo-mode", help="PGO mode recorded in host fingerprint.")
    parser.add_argument("--fingerprint-pgo-profile-data", help="PGO profile path recorded in host fingerprint.")
    parser.add_argument("--fingerprint-pgo-allow-fallback", help="PGO fallback flag recorded in host fingerprint.")
    parser.add_argument("--fingerprint-git-commit", help="Git commit recorded in host fingerprint.")
    parser.add_argument(
        "--fingerprint-git-dirty",
        choices=("true", "false"),
        help="Git dirty flag recorded in host fingerprint.",
    )
    parser.add_argument("--fingerprint-rust-version", help="Rust version recorded in host fingerprint.")
    parser.add_argument("--fingerprint-cargo-runner-mode", help="Resolved cargo runner mode.")
    parser.add_argument("--fingerprint-cargo-runner-request", help="Requested cargo runner mode.")
    parser.add_argument("--fingerprint-correlation-id", help="Correlation ID recorded in host fingerprint.")
    parser.add_argument("--self-test", action="store_true", help="Run disposable self-tests.")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.self_test:
        return run_self_test()
    if args.host_fingerprint:
        write_json(
            build_host_topology_fingerprint(
                proc_root=Path(args.proc_root).expanduser(),
                sys_root=Path(args.sys_root).expanduser(),
                timestamp=args.fingerprint_timestamp,
                build_profile=args.fingerprint_build_profile,
                pgo_mode=args.fingerprint_pgo_mode,
                pgo_profile_data=args.fingerprint_pgo_profile_data,
                pgo_allow_fallback=args.fingerprint_pgo_allow_fallback,
                git_commit=args.fingerprint_git_commit,
                git_dirty=(
                    None
                    if args.fingerprint_git_dirty is None
                    else args.fingerprint_git_dirty == "true"
                ),
                rust_version=args.fingerprint_rust_version,
                cargo_runner_mode=args.fingerprint_cargo_runner_mode,
                cargo_runner_request=args.fingerprint_cargo_runner_request,
                correlation_id=args.fingerprint_correlation_id,
            )
        )
        return 0
    code, payload = build_report(args)
    write_json(payload)
    return code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
