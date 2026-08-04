#!/usr/bin/env python3
"""Verify swarm runpack and evidence bundle freshness.

This is a read-only operator guard. It checks generated runpacks/evidence
artifacts against the source files they cite and against the generator files
that define the runpack contract. It does not mutate Beads, git, Agent Mail,
RCH, runpack inputs, or source files.
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any


REPORT_SCHEMA = "pi.swarm.runpack_freshness_report.v1"
RUNPACK_SCHEMA = "pi.swarm.operator_runpack.v1"
DEFAULT_MAX_AGE_HOURS = 336
MTIME_SKEW_SECONDS = 2.0
PLACEHOLDER_PREFIXES = ("[", "<")
DEFAULT_GENERATOR_SOURCES = (
    "scripts/build_swarm_operator_runpack.py",
    "docs/contracts/swarm-operator-runpack-contract.json",
    "tests/golden_corpus/swarm_operator_runpack/complete_runpack_projection.json",
    "tests/golden_corpus/swarm_operator_runpack/autopilot_plan_projection.json",
)


@dataclass(frozen=True)
class SourceRef:
    source_id: str
    path_text: str
    origin: str
    expected_size: int | None = None
    expected_sha256: str | None = None
    require_fingerprint: bool = False


def utc_now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def as_utc(value: datetime) -> datetime:
    if value.tzinfo is None:
        return value.replace(tzinfo=timezone.utc)
    return value.astimezone(timezone.utc)


def parse_iso_datetime(raw: object) -> datetime | None:
    if not isinstance(raw, str):
        return None
    value = raw.strip()
    if not value:
        return None
    if value.endswith("Z"):
        value = f"{value[:-1]}+00:00"
    try:
        return as_utc(datetime.fromisoformat(value))
    except ValueError:
        return None


def json_dumps(payload: Any, *, pretty: bool = False) -> str:
    if pretty:
        return json.dumps(payload, indent=2, sort_keys=True) + "\n"
    return json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def file_size(path: Path) -> int:
    return path.stat().st_size


def is_placeholder(value: str | None) -> bool:
    if value is None:
        return True
    stripped = value.strip()
    return not stripped or stripped.startswith(PLACEHOLDER_PREFIXES)


def resolve_source_path(path_text: str, source_root: Path) -> Path:
    candidate = Path(path_text)
    if not candidate.is_absolute():
        candidate = source_root / candidate
    return candidate.resolve()


def finding(
    *,
    code: str,
    message: str,
    severity: str = "error",
    source_id: str | None = None,
    path: str | None = None,
    origin: str | None = None,
) -> dict[str, Any]:
    return {
        "severity": severity,
        "code": code,
        "message": message,
        "source_id": source_id,
        "path": path,
        "origin": origin,
    }


def iter_path_values(value: Any) -> list[str]:
    paths: list[str] = []
    if isinstance(value, str):
        paths.append(value)
    elif isinstance(value, list):
        for item in value:
            if isinstance(item, str):
                paths.append(item)
            elif isinstance(item, dict) and isinstance(item.get("path"), str):
                paths.append(item["path"])
    elif isinstance(value, dict) and isinstance(value.get("path"), str):
        paths.append(value["path"])
    return paths


def collect_runpack_sources(payload: dict[str, Any]) -> list[SourceRef]:
    sources: list[SourceRef] = []
    for item in payload.get("source_statuses") or []:
        if not isinstance(item, dict):
            continue
        status = item.get("status")
        if status != "ok":
            continue
        path_text = item.get("path")
        source_id = str(item.get("id") or "unknown")
        expected_size = item.get("size_bytes")
        expected_sha256 = item.get("sha256")
        sources.append(
            SourceRef(
                source_id=source_id,
                path_text=str(path_text) if path_text is not None else "",
                origin="source_statuses",
                expected_size=expected_size if isinstance(expected_size, int) else None,
                expected_sha256=expected_sha256 if isinstance(expected_sha256, str) else None,
                require_fingerprint=True,
            )
        )

    capture = payload.get("capture")
    if isinstance(capture, dict):
        generated_paths = capture.get("generated_source_paths")
        if isinstance(generated_paths, dict):
            for source_id, path_text in generated_paths.items():
                if isinstance(path_text, str):
                    sources.append(
                        SourceRef(
                            source_id=str(source_id),
                            path_text=path_text,
                            origin="capture.generated_source_paths",
                        )
                    )
    return sources


def collect_evidence_bundle_sources(payload: dict[str, Any]) -> list[SourceRef]:
    sources: list[SourceRef] = []
    for index, item in enumerate(payload.get("child_artifact_map") or []):
        if not isinstance(item, dict):
            continue
        source_id = str(item.get("bead_id") or f"child_artifact_map[{index}]")
        for key in ("code_paths", "test_paths", "docs_or_evidence_paths"):
            for path_text in iter_path_values(item.get(key)):
                sources.append(
                    SourceRef(
                        source_id=source_id,
                        path_text=path_text,
                        origin=f"child_artifact_map.{key}",
                    )
                )

    for index, item in enumerate(payload.get("checklist") or []):
        if not isinstance(item, dict):
            continue
        source_id = str(item.get("id") or f"checklist[{index}]")
        for evidence in item.get("evidence") or []:
            for path_text in iter_path_values(evidence):
                sources.append(
                    SourceRef(
                        source_id=source_id,
                        path_text=path_text,
                        origin="checklist.evidence",
                    )
                )
    return sources


def dedupe_sources(sources: list[SourceRef]) -> list[SourceRef]:
    deduped: list[SourceRef] = []
    seen: set[tuple[str, str, str, int | None, str | None, bool]] = set()
    for source in sources:
        key = (
            source.source_id,
            source.path_text,
            source.origin,
            source.expected_size,
            source.expected_sha256,
            source.require_fingerprint,
        )
        if key in seen:
            continue
        seen.add(key)
        deduped.append(source)
    return deduped


def source_refs_for_payload(
    payload: dict[str, Any],
    generator_sources: tuple[str, ...],
) -> list[SourceRef]:
    sources = collect_runpack_sources(payload)
    sources.extend(collect_evidence_bundle_sources(payload))
    for path_text in generator_sources:
        sources.append(
            SourceRef(
                source_id=Path(path_text).name,
                path_text=path_text,
                origin="generator_source",
            )
        )
    return dedupe_sources(sources)


def verify_source_ref(
    source: SourceRef,
    *,
    source_root: Path,
    artifact_mtime: datetime,
) -> tuple[list[dict[str, Any]], bool]:
    findings: list[dict[str, Any]] = []
    checked = False
    if is_placeholder(source.path_text):
        findings.append(
            finding(
                code="source_unknown_path",
                message="source path is absent or placeholder-only",
                source_id=source.source_id,
                path=source.path_text,
                origin=source.origin,
            )
        )
        return findings, checked

    if source.require_fingerprint:
        if source.expected_size is None:
            findings.append(
                finding(
                    code="source_size_missing",
                    message="ok source_statuses entry is missing size_bytes",
                    source_id=source.source_id,
                    path=source.path_text,
                    origin=source.origin,
                )
            )
        if is_placeholder(source.expected_sha256):
            findings.append(
                finding(
                    code="source_sha256_missing",
                    message="ok source_statuses entry is missing a concrete sha256",
                    source_id=source.source_id,
                    path=source.path_text,
                    origin=source.origin,
                )
            )

    source_path = resolve_source_path(source.path_text, source_root)
    if not source_path.exists():
        findings.append(
            finding(
                code="source_missing",
                message="source path referenced by artifact does not exist",
                source_id=source.source_id,
                path=str(source_path),
                origin=source.origin,
            )
        )
        return findings, checked
    if not source_path.is_file():
        findings.append(
            finding(
                code="source_not_file",
                message="source path exists but is not a regular file",
                source_id=source.source_id,
                path=str(source_path),
                origin=source.origin,
            )
        )
        return findings, checked

    checked = True
    source_mtime = datetime.fromtimestamp(source_path.stat().st_mtime, timezone.utc)
    if source_mtime - artifact_mtime > timedelta(seconds=MTIME_SKEW_SECONDS):
        findings.append(
            finding(
                code="source_newer_than_artifact",
                message=(
                    "source file was modified after the generated artifact; "
                    "regenerate the runpack/evidence bundle"
                ),
                source_id=source.source_id,
                path=str(source_path),
                origin=source.origin,
            )
        )

    if source.expected_size is not None:
        observed_size = file_size(source_path)
        if observed_size != source.expected_size:
            findings.append(
                finding(
                    code="source_size_mismatch",
                    message=(
                        f"source size mismatch: expected {source.expected_size}, "
                        f"observed {observed_size}"
                    ),
                    source_id=source.source_id,
                    path=str(source_path),
                    origin=source.origin,
                )
            )

    if source.expected_sha256 is not None and not is_placeholder(source.expected_sha256):
        observed_sha = sha256_file(source_path)
        if observed_sha != source.expected_sha256:
            findings.append(
                finding(
                    code="source_sha256_mismatch",
                    message="source sha256 does not match embedded artifact fingerprint",
                    source_id=source.source_id,
                    path=str(source_path),
                    origin=source.origin,
                )
            )

    return findings, checked


def verify_artifact(
    artifact_path: Path,
    *,
    source_root: Path,
    now: datetime,
    max_age_hours: int,
    generator_sources: tuple[str, ...],
) -> dict[str, Any]:
    result: dict[str, Any] = {
        "artifact_path": str(artifact_path),
        "status": "unknown",
        "schema": None,
        "generated_at": None,
        "source_count": 0,
        "sources_checked": 0,
        "findings": [],
    }
    findings: list[dict[str, Any]] = []
    if not artifact_path.exists():
        findings.append(
            finding(
                code="artifact_missing",
                message="artifact path does not exist",
                path=str(artifact_path),
            )
        )
        result["findings"] = findings
        result["status"] = "fail"
        return result
    if not artifact_path.is_file():
        findings.append(
            finding(
                code="artifact_not_file",
                message="artifact path exists but is not a regular file",
                path=str(artifact_path),
            )
        )
        result["findings"] = findings
        result["status"] = "fail"
        return result

    try:
        payload = json.loads(artifact_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        findings.append(
            finding(
                code="artifact_malformed_json",
                message=f"artifact JSON failed to parse: {exc}",
                path=str(artifact_path),
            )
        )
        result["findings"] = findings
        result["status"] = "fail"
        return result
    if not isinstance(payload, dict):
        findings.append(
            finding(
                code="artifact_not_object",
                message="artifact JSON must be an object",
                path=str(artifact_path),
            )
        )
        result["findings"] = findings
        result["status"] = "fail"
        return result

    schema = payload.get("schema")
    generated_at = parse_iso_datetime(payload.get("generated_at"))
    artifact_mtime = datetime.fromtimestamp(artifact_path.stat().st_mtime, timezone.utc)
    result["schema"] = schema if isinstance(schema, str) else None
    result["generated_at"] = generated_at.isoformat() if generated_at else None

    if generated_at is None:
        findings.append(
            finding(
                code="artifact_generated_at_missing",
                message="artifact is missing a parseable generated_at timestamp",
                path=str(artifact_path),
            )
        )
    elif max_age_hours > 0 and now - generated_at > timedelta(hours=max_age_hours):
        age_hours = (now - generated_at).total_seconds() / 3600
        findings.append(
            finding(
                code="artifact_stale",
                message=(
                    f"artifact generated_at is stale: {age_hours:.1f} hours old "
                    f"(limit: {max_age_hours} hours)"
                ),
                path=str(artifact_path),
            )
        )

    sources = source_refs_for_payload(payload, generator_sources)
    result["source_count"] = len(sources)
    if not sources:
        findings.append(
            finding(
                code="artifact_has_no_sources",
                message="artifact does not expose source_statuses, capture paths, or evidence paths",
                path=str(artifact_path),
            )
        )

    checked_count = 0
    for source in sources:
        source_findings, checked = verify_source_ref(
            source,
            source_root=source_root,
            artifact_mtime=artifact_mtime,
        )
        if checked:
            checked_count += 1
        findings.extend(source_findings)
    result["sources_checked"] = checked_count
    result["findings"] = findings
    result["status"] = "fail" if any(item["severity"] == "error" for item in findings) else "pass"
    return result


def build_report(
    artifacts: list[Path],
    *,
    source_root: Path,
    now: datetime,
    max_age_hours: int,
    generator_sources: tuple[str, ...],
) -> dict[str, Any]:
    artifact_results = [
        verify_artifact(
            artifact,
            source_root=source_root,
            now=now,
            max_age_hours=max_age_hours,
            generator_sources=generator_sources,
        )
        for artifact in artifacts
    ]
    finding_count = sum(len(item["findings"]) for item in artifact_results)
    failed_count = sum(1 for item in artifact_results if item["status"] == "fail")
    return {
        "schema": REPORT_SCHEMA,
        "generated_at": utc_now_iso(),
        "status": "fail" if failed_count else "pass",
        "source_root": str(source_root),
        "max_age_hours": max_age_hours,
        "summary": {
            "artifact_count": len(artifact_results),
            "failed_artifacts": failed_count,
            "finding_count": finding_count,
            "sources_checked": sum(int(item["sources_checked"]) for item in artifact_results),
        },
        "artifacts": artifact_results,
    }


def print_text_report(report: dict[str, Any]) -> None:
    print(f"status={report['status']} artifacts={report['summary']['artifact_count']} findings={report['summary']['finding_count']}")
    for artifact in report["artifacts"]:
        print(
            f"- {artifact['status']}: {artifact['artifact_path']} "
            f"sources_checked={artifact['sources_checked']}/{artifact['source_count']}"
        )
        for item in artifact["findings"]:
            source = f" source={item['source_id']}" if item.get("source_id") else ""
            path = f" path={item['path']}" if item.get("path") else ""
            print(f"  {item['severity']} {item['code']}:{source}{path} {item['message']}")


def write_json(path: Path, payload: Any) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json_dumps(payload, pretty=True), encoding="utf-8")
    return path


def set_mtime(path: Path, timestamp: datetime) -> None:
    seconds = timestamp.timestamp()
    os.utime(path, (seconds, seconds))


def assert_report_status(report: dict[str, Any], expected: str, code: str | None = None) -> None:
    if report["status"] != expected:
        print(json_dumps(report, pretty=True))
        raise AssertionError(f"expected report status {expected}, got {report['status']}")
    if code is not None:
        codes = {
            finding["code"]
            for artifact in report["artifacts"]
            for finding in artifact["findings"]
        }
        if code not in codes:
            print(json_dumps(report, pretty=True))
            raise AssertionError(f"expected finding code {code!r}, got {sorted(codes)!r}")


def first_json_object(text: str) -> dict[str, Any] | None:
    decoder = json.JSONDecoder()
    for index, character in enumerate(text):
        if character != "{":
            continue
        try:
            payload, _ = decoder.raw_decode(text[index:])
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict):
            return payload
    return None


def fixture_runpack(
    *,
    artifact_path: Path,
    source_path: Path,
    generated_at: datetime,
    path_text: str | None = None,
    sha_override: str | None = None,
    size_override: int | None = None,
) -> Path:
    path_text = path_text or str(source_path)
    size = file_size(source_path) if size_override is None else size_override
    sha = sha256_file(source_path) if sha_override is None else sha_override
    return write_json(
        artifact_path,
        {
            "schema": RUNPACK_SCHEMA,
            "generated_at": generated_at.isoformat(),
            "source_statuses": [
                {
                    "id": "doctor_swarm",
                    "path": path_text,
                    "status": "ok",
                    "schema": "fixture.source.v1",
                    "size_bytes": size,
                    "sha256": sha,
                }
            ],
            "capture": {
                "schema": "pi.swarm.operator_runpack_capture.v1",
                "generated_at": generated_at.isoformat(),
                "generated_source_paths": {
                    "doctor_swarm": path_text,
                },
            },
        },
    )


def prepare_fixture_repo(root: Path, generated_at: datetime) -> tuple[Path, Path]:
    generator_files = [
        root / "scripts/build_swarm_operator_runpack.py",
        root / "docs/contracts/swarm-operator-runpack-contract.json",
        root / "tests/golden_corpus/swarm_operator_runpack/complete_runpack_projection.json",
        root / "tests/golden_corpus/swarm_operator_runpack/autopilot_plan_projection.json",
    ]
    for index, path in enumerate(generator_files):
        write_json(path, {"fixture": index, "generated_at": generated_at.isoformat()})
        set_mtime(path, generated_at - timedelta(hours=3))
    source_path = write_json(
        root / "sources/doctor.json",
        {"schema": "fixture.source.v1", "generated_at": generated_at.isoformat()},
    )
    set_mtime(source_path, generated_at - timedelta(hours=2))
    return root, source_path


def run_self_test() -> int:
    now = datetime(2026, 5, 15, 12, 0, 0, tzinfo=timezone.utc)
    generated_at = now - timedelta(hours=1)
    with tempfile.TemporaryDirectory(prefix="pi_runpack_freshness_") as temp_dir:
        root, source_path = prepare_fixture_repo(Path(temp_dir), generated_at)
        artifact = fixture_runpack(
            artifact_path=root / "artifacts/runpack.json",
            source_path=source_path,
            generated_at=generated_at,
        )
        set_mtime(artifact, generated_at)
        report = build_report(
            [artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(report, "pass")

        set_mtime(source_path, generated_at + timedelta(minutes=5))
        stale_report = build_report(
            [artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(stale_report, "fail", "source_newer_than_artifact")
        set_mtime(source_path, generated_at - timedelta(hours=2))

        missing_artifact = fixture_runpack(
            artifact_path=root / "artifacts/missing-source.json",
            source_path=source_path,
            generated_at=generated_at,
            path_text=str(root / "sources/missing.json"),
        )
        set_mtime(missing_artifact, generated_at)
        missing_report = build_report(
            [missing_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(missing_report, "fail", "source_missing")

        unknown_artifact = fixture_runpack(
            artifact_path=root / "artifacts/unknown-source.json",
            source_path=source_path,
            generated_at=generated_at,
            path_text="[WORKSPACE]/doctor.json",
            sha_override="[SHA256]",
        )
        set_mtime(unknown_artifact, generated_at)
        unknown_report = build_report(
            [unknown_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(unknown_report, "fail", "source_unknown_path")

        mismatch_artifact = fixture_runpack(
            artifact_path=root / "artifacts/hash-mismatch.json",
            source_path=source_path,
            generated_at=generated_at,
            sha_override="0" * 64,
        )
        set_mtime(mismatch_artifact, generated_at)
        mismatch_report = build_report(
            [mismatch_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(mismatch_report, "fail", "source_sha256_mismatch")

        missing_fingerprint_artifact = write_json(
            root / "artifacts/missing-fingerprint.json",
            {
                "schema": RUNPACK_SCHEMA,
                "generated_at": generated_at.isoformat(),
                "source_statuses": [
                    {
                        "id": "doctor_swarm",
                        "path": str(source_path),
                        "status": "ok",
                    }
                ],
            },
        )
        set_mtime(missing_fingerprint_artifact, generated_at)
        missing_fingerprint_report = build_report(
            [missing_fingerprint_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(missing_fingerprint_report, "fail", "source_size_missing")
        assert_report_status(missing_fingerprint_report, "fail", "source_sha256_missing")

        size_mismatch_artifact = fixture_runpack(
            artifact_path=root / "artifacts/size-mismatch.json",
            source_path=source_path,
            generated_at=generated_at,
            size_override=0,
        )
        set_mtime(size_mismatch_artifact, generated_at)
        size_mismatch_report = build_report(
            [size_mismatch_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(size_mismatch_report, "fail", "source_size_mismatch")

        malformed_artifact = root / "artifacts/malformed.json"
        malformed_artifact.write_text("{", encoding="utf-8")
        set_mtime(malformed_artifact, generated_at)
        malformed_report = build_report(
            [malformed_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(malformed_report, "fail", "artifact_malformed_json")

        not_object_artifact = write_json(root / "artifacts/not-object.json", ["not", "object"])
        set_mtime(not_object_artifact, generated_at)
        not_object_report = build_report(
            [not_object_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(not_object_report, "fail", "artifact_not_object")

        no_sources_artifact = write_json(
            root / "artifacts/no-sources.json",
            {
                "schema": RUNPACK_SCHEMA,
                "generated_at": generated_at.isoformat(),
            },
        )
        set_mtime(no_sources_artifact, generated_at)
        no_sources_report = build_report(
            [no_sources_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=(),
        )
        assert_report_status(no_sources_report, "fail", "artifact_has_no_sources")

        missing_generator_report = build_report(
            [artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=("scripts/missing-generator.py",),
        )
        assert_report_status(missing_generator_report, "fail", "source_missing")

        stale_age_artifact = fixture_runpack(
            artifact_path=root / "artifacts/stale-age.json",
            source_path=source_path,
            generated_at=now - timedelta(days=30),
        )
        set_mtime(stale_age_artifact, generated_at)
        stale_age_report = build_report(
            [stale_age_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(stale_age_report, "fail", "artifact_stale")

        code_path = write_json(root / "src/swarm_progress_slo.rs", {"ok": True})
        test_path = write_json(root / "tests/swarm_progress_slo_contract.rs", {"ok": True})
        docs_path = write_json(root / "docs/contracts/swarm-progress-slo-contract.json", {"ok": True})
        for path in (code_path, test_path, docs_path):
            set_mtime(path, generated_at - timedelta(minutes=30))
        closeout_artifact = write_json(
            root / "artifacts/closeout.json",
            {
                "schema": "pi.swarm.progress_slo.closeout_gate.v1",
                "generated_at": generated_at.isoformat(),
                "child_artifact_map": [
                    {
                        "bead_id": "bd-fixture",
                        "code_paths": ["src/swarm_progress_slo.rs"],
                        "test_paths": ["tests/swarm_progress_slo_contract.rs"],
                        "docs_or_evidence_paths": [
                            "docs/contracts/swarm-progress-slo-contract.json"
                        ],
                    }
                ],
                "checklist": [
                    {
                        "id": "contract",
                        "evidence": [
                            {"path": "docs/contracts/swarm-progress-slo-contract.json"}
                        ],
                    }
                ],
            },
        )
        set_mtime(closeout_artifact, generated_at)
        closeout_report = build_report(
            [closeout_artifact],
            source_root=root,
            now=now,
            max_age_hours=DEFAULT_MAX_AGE_HOURS,
            generator_sources=DEFAULT_GENERATOR_SOURCES,
        )
        assert_report_status(closeout_report, "pass")

    print("SELF-TEST PASS")
    return 0


def run_runpack_smoke(repo_root: Path) -> int:
    script = repo_root / "scripts/build_swarm_operator_runpack.py"
    if not script.exists():
        print(f"ERROR: runpack builder not found: {script}", file=sys.stderr)
        return 2
    completed = subprocess.run(
        [sys.executable, str(script), "--self-test"],
        cwd=repo_root,
        text=True,
        capture_output=True,
        timeout=300,
        check=False,
    )
    if completed.returncode != 0:
        sys.stdout.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        print("ERROR: runpack builder self-test failed", file=sys.stderr)
        return 1
    payload = first_json_object(completed.stdout)
    if payload is None:
        print("ERROR: runpack builder self-test did not emit JSON", file=sys.stderr)
        return 2
    workspace = payload.get("workspace")
    if not isinstance(workspace, str):
        print("ERROR: runpack builder self-test JSON missing workspace", file=sys.stderr)
        return 2
    runpack_path = Path(workspace) / "runpack.json"
    runpack_payload = payload.get("runpack")
    generated_at = None
    if isinstance(runpack_payload, dict):
        generated_at = parse_iso_datetime(runpack_payload.get("generated_at"))
    now = generated_at + timedelta(hours=1) if generated_at else datetime.now(timezone.utc)
    report = build_report(
        [runpack_path],
        source_root=repo_root,
        now=now,
        max_age_hours=DEFAULT_MAX_AGE_HOURS,
        generator_sources=DEFAULT_GENERATOR_SOURCES,
    )
    if report["status"] != "pass":
        print_text_report(report)
        return 1
    print("RUNPACK-SMOKE PASS")
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "artifacts",
        nargs="*",
        type=Path,
        help="runpack/evidence JSON artifacts to verify",
    )
    parser.add_argument(
        "--source-root",
        type=Path,
        default=Path("."),
        help="repository root used to resolve relative source paths",
    )
    parser.add_argument(
        "--max-age-hours",
        type=int,
        default=DEFAULT_MAX_AGE_HOURS,
        help="maximum generated_at age; 0 disables age checks",
    )
    parser.add_argument(
        "--now",
        help="override current time for deterministic tests",
    )
    parser.add_argument(
        "--generator-source",
        action="append",
        default=[],
        help="additional generator script/contract/test path to check",
    )
    parser.add_argument(
        "--skip-default-generator-sources",
        action="store_true",
        help="do not require default runpack script/contract/golden generator sources",
    )
    parser.add_argument("--json", action="store_true", help="print machine-readable JSON")
    parser.add_argument("--self-test", action="store_true", help="run fixture-backed self-test")
    parser.add_argument(
        "--run-runpack-smoke",
        action="store_true",
        help="run build_swarm_operator_runpack.py --self-test and verify its runpack output",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        return run_self_test()
    source_root = args.source_root.resolve()
    if args.run_runpack_smoke:
        return run_runpack_smoke(source_root)
    if not args.artifacts:
        print("ERROR: provide at least one artifact path or use --self-test", file=sys.stderr)
        return 2
    if args.max_age_hours < 0:
        print("ERROR: --max-age-hours must be >= 0", file=sys.stderr)
        return 2
    now = parse_iso_datetime(args.now) if args.now else datetime.now(timezone.utc)
    if now is None:
        print("ERROR: --now must be an ISO-8601 timestamp", file=sys.stderr)
        return 2
    default_generators = () if args.skip_default_generator_sources else DEFAULT_GENERATOR_SOURCES
    generator_sources = tuple(default_generators) + tuple(args.generator_source or [])
    report = build_report(
        [artifact.resolve() for artifact in args.artifacts],
        source_root=source_root,
        now=now,
        max_age_hours=args.max_age_hours,
        generator_sources=generator_sources,
    )
    if args.json:
        print(json_dumps(report, pretty=True), end="")
    else:
        print_text_report(report)
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    with contextlib.suppress(BrokenPipeError):
        sys.exit(main())
