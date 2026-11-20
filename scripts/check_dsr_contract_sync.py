#!/usr/bin/env python3
"""Validate DSR quality-registry and build-authority contract parity (bd-dsr-authority-dist-size-budget-dk9av).

Enforces exact structural bijection across:
  1. Cargo.toml ([package].name, [[bin]].name, [profile.dist])
  2. .dsr/repos.yaml (tools.pi_agent_rust)
  3. .dsr/repos.d/pi_agent_rust.yaml (build_profile: dist, overlapping identity keys, no retired workflow)
  4. Optional host ~/.config/dsr/ parity (--check-host / --check-host-if-present / PI_CHECK_HOST_DSR=1)

Outputs structured JSON diagnostics (`schema: "pi.dsr.contract_sync.v1"`) on `--json` or failure.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
import tomllib
from pathlib import Path
from typing import Any


SCHEMA_ID = "pi.dsr.contract_sync.v1"
TOOL_KEY = "pi_agent_rust"
SHARED_KEYS = ("repo", "local_path", "language", "binary_name", "targets")
BUILD_ONLY_KEYS = (
    "build_cmd",
    "build_profile",
    "target_triples",
    "cross_compile",
    "release_contract",
    "workspace_additional_artifacts",
)
REQUIRED_CHECK_SUBSTRINGS = (
    ("fmt_check", "cargo fmt --check"),
    ("cargo_check_keep_going", "cargo check"),
    ("cargo_clippy_keep_going", "cargo clippy"),
    ("wasm_host_lane", "--features wasm-host"),
    ("cargo_test_all_targets", "cargo test"),
    ("installer_regression", "tests/installer_regression.sh"),
    ("module_reachability", "scripts/check_module_reachability.py"),
    ("fixture_read_patience", "scripts/check_fixture_read_patience.py"),
    ("readme_evidence_freshness", "scripts/check_readme_evidence_freshness.py"),
    ("dsr_contract_sync", "scripts/check_dsr_contract_sync.py"),
)


def _strip_yaml_scalar(raw: str) -> Any:
    val = raw.strip()
    if not val:
        return ""
    if val in ("null", "~"):
        return None
    if val == "true":
        return True
    if val == "false":
        return False
    if val == "{}":
        return {}
    if val == "[]":
        return []
    if val.startswith("[") and val.endswith("]"):
        inner = val[1:-1].strip()
        if not inner:
            return []
        return [_strip_yaml_scalar(part) for part in inner.split(",")]
    if (val.startswith('"') and val.endswith('"')) or (
        val.startswith("'") and val.endswith("'")
    ):
        return val[1:-1]
    if val.isdigit():
        return int(val)
    return val


def _fallback_parse_yaml(text: str) -> dict[str, Any]:
    """Minimal indentation-based YAML subset parser for .dsr/*.yaml on hosts lacking PyYAML."""
    lines: list[tuple[int, str]] = []
    for raw_line in text.splitlines():
        stripped = raw_line.lstrip()
        if not stripped or stripped.startswith("#"):
            continue
        indent = len(raw_line) - len(stripped)
        # Strip trailing comments outside quotes.
        in_single = False
        in_double = False
        clean_chars: list[str] = []
        for idx, ch in enumerate(stripped):
            if ch == "'" and not in_double:
                in_single = not in_single
            elif ch == '"' and not in_single:
                in_double = not in_double
            elif ch == "#" and not in_single and not in_double:
                if idx == 0 or stripped[idx - 1].isspace():
                    break
            clean_chars.append(ch)
        cleaned = "".join(clean_chars).rstrip()
        if cleaned:
            lines.append((indent, cleaned))

    def parse_block(index: int, min_indent: int) -> tuple[Any, int]:
        if index >= len(lines):
            return {}, index
        first_indent, first_text = lines[index]
        if first_indent < min_indent:
            return {}, index
        if first_text.startswith("- "):
            items: list[Any] = []
            while index < len(lines):
                indent, content = lines[index]
                if indent < first_indent or not content.startswith("- "):
                    break
                if indent > first_indent:
                    break
                items.append(_strip_yaml_scalar(content[2:]))
                index += 1
            return items, index

        mapping: dict[str, Any] = {}
        while index < len(lines):
            indent, content = lines[index]
            if indent < first_indent:
                break
            if indent > first_indent or content.startswith("- "):
                break
            key, sep, rest = content.partition(":")
            if not sep:
                index += 1
                continue
            key = key.strip()
            rest = rest.strip()
            index += 1
            if rest in (">-", ">", "|", "|-"):
                folded: list[str] = []
                while index < len(lines) and lines[index][0] > indent:
                    folded.append(lines[index][1])
                    index += 1
                joiner = " " if rest.startswith(">") else "\n"
                mapping[key] = joiner.join(folded)
            elif rest == "":
                if index < len(lines) and lines[index][0] > indent:
                    child, index = parse_block(index, lines[index][0])
                    mapping[key] = child
                else:
                    mapping[key] = None
            else:
                mapping[key] = _strip_yaml_scalar(rest)
        return mapping, index

    parsed, _ = parse_block(0, 0)
    return parsed if isinstance(parsed, dict) else {}


def load_yaml_file(path: Path) -> dict[str, Any]:
    text = path.read_text(encoding="utf-8")
    try:
        import yaml  # type: ignore[import-not-found]

        loaded = yaml.safe_load(text)
        return loaded if isinstance(loaded, dict) else {}
    except ImportError:
        return _fallback_parse_yaml(text)


def add_diag(
    diagnostics: list[dict[str, Any]],
    *,
    code: str,
    file: Path,
    field: str,
    expected: Any,
    observed: Any,
    remediation: str,
) -> None:
    diagnostics.append(
        {
            "code": code,
            "file": str(file),
            "field": field,
            "expected": expected,
            "observed": observed,
            "remediation": remediation,
        }
    )


def evaluate_contract(
    repo_root: Path,
    host_dsr_dir: Path | None,
    require_host: bool,
) -> dict[str, Any]:
    diagnostics: list[dict[str, Any]] = []
    checked_files: list[str] = []

    cargo_toml_path = repo_root / "Cargo.toml"
    repo_registry_path = repo_root / ".dsr" / "repos.yaml"
    repo_authority_path = repo_root / ".dsr" / "repos.d" / "pi_agent_rust.yaml"

    for required_path in (cargo_toml_path, repo_registry_path, repo_authority_path):
        checked_files.append(str(required_path))
        if not required_path.is_file():
            add_diag(
                diagnostics,
                code="missing_repo_contract_file",
                file=required_path,
                field="exists",
                expected=True,
                observed=False,
                remediation=f"Restore checked-in file {required_path}.",
            )

    if diagnostics:
        return {
            "schema": SCHEMA_ID,
            "status": "fail",
            "checked_files": checked_files,
            "drift_count": len(diagnostics),
            "diagnostics": diagnostics,
        }

    cargo_toml = tomllib.loads(cargo_toml_path.read_text(encoding="utf-8"))
    pkg_name = cargo_toml.get("package", {}).get("name")
    if pkg_name != TOOL_KEY:
        add_diag(
            diagnostics,
            code="cargo_package_name_mismatch",
            file=cargo_toml_path,
            field="package.name",
            expected=TOOL_KEY,
            observed=pkg_name,
            remediation="Keep [package].name == 'pi_agent_rust' in Cargo.toml.",
        )

    bins = cargo_toml.get("bin", [])
    bin_names = [b.get("name") for b in bins if isinstance(b, dict)]
    if "pi" not in bin_names:
        add_diag(
            diagnostics,
            code="cargo_bin_name_missing",
            file=cargo_toml_path,
            field="bin[].name",
            expected="pi",
            observed=bin_names,
            remediation="Declare [[bin]] name = 'pi' in Cargo.toml.",
        )

    dist_profile = cargo_toml.get("profile", {}).get("dist")
    if not isinstance(dist_profile, dict) or dist_profile.get("inherits") != "release":
        add_diag(
            diagnostics,
            code="cargo_dist_profile_invalid",
            file=cargo_toml_path,
            field="profile.dist",
            expected={"inherits": "release"},
            observed=dist_profile,
            remediation="Define [profile.dist] inheriting from 'release' in Cargo.toml.",
        )

    repo_registry_doc = load_yaml_file(repo_registry_path)
    repo_tool = (repo_registry_doc.get("tools") or {}).get(TOOL_KEY)
    if not isinstance(repo_tool, dict):
        add_diag(
            diagnostics,
            code="repo_registry_missing_tool",
            file=repo_registry_path,
            field=f"tools.{TOOL_KEY}",
            expected="mapping",
            observed=type(repo_tool).__name__,
            remediation="Define tools.pi_agent_rust in .dsr/repos.yaml.",
        )
        repo_tool = {}

    repo_authority = load_yaml_file(repo_authority_path)
    if repo_authority.get("tool_name") != TOOL_KEY:
        add_diag(
            diagnostics,
            code="repo_authority_tool_name_mismatch",
            file=repo_authority_path,
            field="tool_name",
            expected=TOOL_KEY,
            observed=repo_authority.get("tool_name"),
            remediation="Set tool_name: pi_agent_rust in .dsr/repos.d/pi_agent_rust.yaml.",
        )

    if repo_authority.get("build_profile") != "dist":
        add_diag(
            diagnostics,
            code="repo_authority_build_profile_mismatch",
            file=repo_authority_path,
            field="build_profile",
            expected="dist",
            observed=repo_authority.get("build_profile"),
            remediation="Align build_profile: dist in .dsr/repos.d/pi_agent_rust.yaml.",
        )

    build_cmd = str(repo_authority.get("build_cmd") or "")
    if "--profile dist" not in build_cmd:
        add_diag(
            diagnostics,
            code="repo_authority_build_cmd_profile_mismatch",
            file=repo_authority_path,
            field="build_cmd",
            expected="contains '--profile dist'",
            observed=build_cmd,
            remediation="Build shipping binaries with --profile dist in .dsr/repos.d/pi_agent_rust.yaml.",
        )

    if "PI_BUILD_GIT_SHA" not in build_cmd:
        add_diag(
            diagnostics,
            code="repo_authority_build_cmd_missing_git_sha",
            file=repo_authority_path,
            field="build_cmd",
            expected="propagates PI_BUILD_GIT_SHA",
            observed=build_cmd,
            remediation="Propagate PI_BUILD_GIT_SHA in .dsr/repos.d/pi_agent_rust.yaml build_cmd so pi --version never bakes 'unknown'.",
        )

    win_cross = (repo_authority.get("cross_compile") or {}).get("windows/amd64")
    win_build_cmd = str((win_cross or {}).get("build_cmd") or "") if isinstance(win_cross, dict) else ""
    if win_build_cmd and "PI_BUILD_GIT_SHA" not in win_build_cmd:
        add_diag(
            diagnostics,
            code="repo_authority_win_build_cmd_missing_git_sha",
            file=repo_authority_path,
            field="cross_compile.windows/amd64.build_cmd",
            expected="propagates PI_BUILD_GIT_SHA",
            observed=win_build_cmd,
            remediation="Propagate PI_BUILD_GIT_SHA in Windows cross_compile build_cmd in .dsr/repos.d/pi_agent_rust.yaml.",
        )

    auth_env = repo_authority.get("env")
    if not isinstance(auth_env, dict) or str(auth_env.get("PI_REQUIRE_GIT_SHA")) != "1":
        add_diag(
            diagnostics,
            code="repo_authority_missing_require_git_sha",
            file=repo_authority_path,
            field="env.PI_REQUIRE_GIT_SHA",
            expected="1",
            observed=auth_env.get("PI_REQUIRE_GIT_SHA") if isinstance(auth_env, dict) else auth_env,
            remediation='Set env.PI_REQUIRE_GIT_SHA: "1" in .dsr/repos.d/pi_agent_rust.yaml so release builds fail closed without a commit SHA.',
        )

    if "workflow" in repo_authority:
        add_diag(
            diagnostics,
            code="retired_workflow_key_present",
            file=repo_authority_path,
            field="workflow",
            expected="<absent>",
            observed=repo_authority.get("workflow"),
            remediation="Remove retired GitHub Actions workflow key from .dsr/repos.d/pi_agent_rust.yaml.",
        )

    if "checks" in repo_authority:
        add_diag(
            diagnostics,
            code="checks_in_build_authority",
            file=repo_authority_path,
            field="checks",
            expected="<absent in repos.d>",
            observed="present",
            remediation="Keep quality checks exclusively in .dsr/repos.yaml (repos.d is build/release authority).",
        )

    for build_key in BUILD_ONLY_KEYS:
        if build_key in repo_tool:
            add_diag(
                diagnostics,
                code="build_key_in_registry",
                file=repo_registry_path,
                field=f"tools.{TOOL_KEY}.{build_key}",
                expected="<absent in repos.yaml>",
                observed=repo_tool.get(build_key),
                remediation=f"Keep {build_key} exclusively in .dsr/repos.d/pi_agent_rust.yaml.",
            )

    for shared_key in SHARED_KEYS:
        reg_val = repo_tool.get(shared_key)
        auth_val = repo_authority.get(shared_key)
        if reg_val != auth_val:
            add_diag(
                diagnostics,
                code="shared_key_divergence",
                file=repo_registry_path,
                field=shared_key,
                expected=auth_val,
                observed=reg_val,
                remediation=f"Align {shared_key} identically across .dsr/repos.yaml and .dsr/repos.d/pi_agent_rust.yaml.",
            )

    if repo_authority.get("binary_name") != "pi":
        add_diag(
            diagnostics,
            code="binary_name_cargo_mismatch",
            file=repo_authority_path,
            field="binary_name",
            expected="pi",
            observed=repo_authority.get("binary_name"),
            remediation="Align binary_name: pi with Cargo.toml [[bin]].",
        )

    checks = repo_tool.get("checks")
    if not isinstance(checks, list) or not checks:
        add_diag(
            diagnostics,
            code="repo_registry_checks_empty",
            file=repo_registry_path,
            field=f"tools.{TOOL_KEY}.checks",
            expected="non-empty list",
            observed=checks,
            remediation="Populate quality checks under tools.pi_agent_rust.checks in .dsr/repos.yaml.",
        )
        checks_strs: list[str] = []
    else:
        checks_strs = [str(item) for item in checks]

    for check_id, needle in REQUIRED_CHECK_SUBSTRINGS:
        if not any(needle in cmd for cmd in checks_strs):
            add_diag(
                diagnostics,
                code=f"missing_required_check_{check_id}",
                file=repo_registry_path,
                field=f"tools.{TOOL_KEY}.checks",
                expected=f"check containing {needle!r}",
                observed=checks_strs,
                remediation=f"Register {needle!r} in .dsr/repos.yaml tools.{TOOL_KEY}.checks.",
            )

    for kind in ("cargo check", "cargo clippy"):
        matching = [cmd for cmd in checks_strs if kind in cmd]
        if matching and not all("--keep-going" in cmd for cmd in matching):
            add_diag(
                diagnostics,
                code="missing_keep_going_flag",
                file=repo_registry_path,
                field=f"tools.{TOOL_KEY}.checks",
                expected=f"all {kind!r} checks carry '--keep-going'",
                observed=matching,
                remediation=f"Add --keep-going to every {kind} check in .dsr/repos.yaml.",
            )

    test_all = [cmd for cmd in checks_strs if "cargo test" in cmd and "--all-targets" in cmd]
    if not test_all or not all("--no-fail-fast" in cmd for cmd in test_all):
        add_diag(
            diagnostics,
            code="missing_no_fail_fast_flag",
            file=repo_registry_path,
            field=f"tools.{TOOL_KEY}.checks",
            expected="cargo test --all-targets check carries '--no-fail-fast'",
            observed=test_all,
            remediation="Add --no-fail-fast to the all-targets cargo test check in .dsr/repos.yaml.",
        )

    if host_dsr_dir is not None:
        host_registry_path = host_dsr_dir / "repos.yaml"
        host_authority_path = host_dsr_dir / "repos.d" / "pi_agent_rust.yaml"
        host_present = host_registry_path.exists() or host_authority_path.exists()
        if require_host or host_present:
            checked_files.extend([str(host_registry_path), str(host_authority_path)])
            if not host_registry_path.is_file():
                add_diag(
                    diagnostics,
                    code="missing_host_registry",
                    file=host_registry_path,
                    field="exists",
                    expected=True,
                    observed=False,
                    remediation=(
                        "Install host DSR registry via: "
                        "yq -i '.tools.pi_agent_rust = load(\".dsr/repos.yaml\").tools.pi_agent_rust' ~/.config/dsr/repos.yaml"
                    ),
                )
            if not host_authority_path.is_file():
                add_diag(
                    diagnostics,
                    code="missing_host_authority",
                    file=host_authority_path,
                    field="exists",
                    expected=True,
                    observed=False,
                    remediation="Copy .dsr/repos.d/pi_agent_rust.yaml to ~/.config/dsr/repos.d/pi_agent_rust.yaml.",
                )
            if host_registry_path.is_file() and host_authority_path.is_file():
                host_reg_doc = load_yaml_file(host_registry_path)
                host_tool = (host_reg_doc.get("tools") or {}).get(TOOL_KEY)
                host_auth = load_yaml_file(host_authority_path)
                if not isinstance(host_tool, dict):
                    add_diag(
                        diagnostics,
                        code="host_registry_missing_tool",
                        file=host_registry_path,
                        field=f"tools.{TOOL_KEY}",
                        expected="mapping",
                        observed=type(host_tool).__name__,
                        remediation="Merge .dsr/repos.yaml tools.pi_agent_rust into ~/.config/dsr/repos.yaml.",
                    )
                else:
                    host_checks = [str(c) for c in (host_tool.get("checks") or [])]
                    if host_checks != checks_strs:
                        add_diag(
                            diagnostics,
                            code="host_registry_checks_drift",
                            file=host_registry_path,
                            field=f"tools.{TOOL_KEY}.checks",
                            expected=checks_strs,
                            observed=host_checks,
                            remediation=(
                                "Sync ~/.config/dsr/repos.yaml checks via: "
                                "yq -i '.tools.pi_agent_rust = load(\".dsr/repos.yaml\").tools.pi_agent_rust' ~/.config/dsr/repos.yaml"
                            ),
                        )
                    if host_tool.get("local_path") != host_auth.get("local_path"):
                        add_diag(
                            diagnostics,
                            code="host_local_path_divergence",
                            file=host_registry_path,
                            field=f"tools.{TOOL_KEY}.local_path",
                            expected=host_auth.get("local_path"),
                            observed=host_tool.get("local_path"),
                            remediation="Align local_path across ~/.config/dsr/repos.yaml and ~/.config/dsr/repos.d/pi_agent_rust.yaml.",
                        )

                for auth_key in (
                    "tool_name",
                    "repo",
                    "language",
                    "binary_name",
                    "build_profile",
                    "build_cmd",
                    "cross_compile",
                    "targets",
                    "target_triples",
                    "release_contract",
                    "env",
                ):
                    if host_auth.get(auth_key) != repo_authority.get(auth_key):
                        add_diag(
                            diagnostics,
                            code="host_authority_drift",
                            file=host_authority_path,
                            field=auth_key,
                            expected=repo_authority.get(auth_key),
                            observed=host_auth.get(auth_key),
                            remediation="Sync .dsr/repos.d/pi_agent_rust.yaml to ~/.config/dsr/repos.d/pi_agent_rust.yaml.",
                        )
                if "workflow" in host_auth:
                    add_diag(
                        diagnostics,
                        code="host_retired_workflow_key",
                        file=host_authority_path,
                        field="workflow",
                        expected="<absent>",
                        observed=host_auth.get("workflow"),
                        remediation="Remove retired workflow key from ~/.config/dsr/repos.d/pi_agent_rust.yaml.",
                    )

    return {
        "schema": SCHEMA_ID,
        "status": "pass" if not diagnostics else "fail",
        "checked_files": checked_files,
        "drift_count": len(diagnostics),
        "diagnostics": diagnostics,
    }


def _write_valid_fixture(repo_dir: Path, host_dir: Path) -> None:
    (repo_dir / ".dsr" / "repos.d").mkdir(parents=True, exist_ok=True)
    (host_dir / "repos.d").mkdir(parents=True, exist_ok=True)
    (repo_dir / "Cargo.toml").write_text(
        '[package]\nname = "pi_agent_rust"\nversion = "0.8.0"\n\n'
        '[[bin]]\nname = "pi"\npath = "src/main.rs"\n\n'
        '[profile.dist]\ninherits = "release"\nopt-level = 3\n',
        encoding="utf-8",
    )
    repos_yaml = (
        'schema_version: "1.0.0"\n'
        "tools:\n"
        "  pi_agent_rust:\n"
        "    repo: Dicklesworthstone/pi_agent_rust\n"
        "    local_path: /data/projects/pi_agent_rust\n"
        "    language: rust\n"
        "    binary_name: pi\n"
        "    targets:\n"
        "      - linux/amd64\n"
        "      - windows/amd64\n"
        "    checks:\n"
        "      - cargo fmt --check\n"
        "      - cargo check --locked --all-targets --keep-going\n"
        "      - cargo clippy --locked --all-targets --keep-going -- -D warnings\n"
        "      - cargo clippy --locked --lib --features wasm-host --keep-going -- -D warnings\n"
        "      - cargo test --locked --all-targets --no-fail-fast\n"
        "      - bash tests/installer_regression.sh\n"
        "      - python3 scripts/check_module_reachability.py\n"
        "      - python3 scripts/check_fixture_read_patience.py\n"
        "      - python3 scripts/check_readme_evidence_freshness.py --structural-only\n"
        "      - python3 scripts/check_dsr_contract_sync.py --check-host-if-present\n"
    )
    authority_yaml = (
        "tool_name: pi_agent_rust\n"
        "repo: Dicklesworthstone/pi_agent_rust\n"
        "local_path: /data/projects/pi_agent_rust\n"
        "language: rust\n"
        "binary_name: pi\n"
        'build_cmd: PI_BUILD_GIT_SHA="$(git rev-parse HEAD)" cargo build --locked --profile dist --bin pi\n'
        "build_profile: dist\n"
        "targets:\n"
        "  - linux/amd64\n"
        "  - windows/amd64\n"
        "target_triples:\n"
        "  linux/amd64: x86_64-unknown-linux-gnu\n"
        "  windows/amd64: x86_64-pc-windows-msvc\n"
        "cross_compile:\n"
        "  windows/amd64:\n"
        "    host: wsurf\n"
        '    build_cmd: set "PI_BUILD_GIT_SHA=0123456789abcdef0123456789abcdef01234567" && cargo build --locked --profile dist --bin pi\n'
        "release_contract:\n"
        "  checksum_sidecar: sha256\n"
        "env:\n"
        '  CARGO_INCREMENTAL: "0"\n'
        '  PI_REQUIRE_GIT_SHA: "1"\n'
    )
    (repo_dir / ".dsr" / "repos.yaml").write_text(repos_yaml, encoding="utf-8")
    (repo_dir / ".dsr" / "repos.d" / "pi_agent_rust.yaml").write_text(
        authority_yaml, encoding="utf-8"
    )
    (host_dir / "repos.yaml").write_text(repos_yaml, encoding="utf-8")
    (host_dir / "repos.d" / "pi_agent_rust.yaml").write_text(
        authority_yaml, encoding="utf-8"
    )


def run_self_test() -> int:
    with tempfile.TemporaryDirectory(prefix="pi-dsr-contract-") as tmp:
        root = Path(tmp)
        repo_dir = root / "repo"
        host_dir = root / "host"
        _write_valid_fixture(repo_dir, host_dir)

        clean = evaluate_contract(repo_dir, host_dir, require_host=True)
        assert clean["status"] == "pass", f"clean fixture failed: {clean}"

        # Planted negative 1: legacy tool_name: pi, build_profile: release, workflow key
        (host_dir / "repos.d" / "pi_agent_rust.yaml").write_text(
            (repo_dir / ".dsr" / "repos.d" / "pi_agent_rust.yaml")
            .read_text(encoding="utf-8")
            .replace("tool_name: pi_agent_rust", "tool_name: pi")
            .replace("build_profile: dist", "build_profile: release")
            + "workflow: .github/workflows/release.yml\n",
            encoding="utf-8",
        )
        res1 = evaluate_contract(repo_dir, host_dir, require_host=True)
        codes1 = {d["code"] for d in res1["diagnostics"]}
        assert res1["status"] == "fail", "expected legacy host authority to fail"
        assert "host_authority_drift" in codes1, codes1
        assert "host_retired_workflow_key" in codes1, codes1

        # Planted negative 2: host registry missing checks / drifted local_path
        _write_valid_fixture(repo_dir, host_dir)
        (host_dir / "repos.yaml").write_text(
            "tools:\n  pi_agent_rust:\n    local_path: /wrong/path\n    checks: []\n",
            encoding="utf-8",
        )
        res2 = evaluate_contract(repo_dir, host_dir, require_host=True)
        codes2 = {d["code"] for d in res2["diagnostics"]}
        assert res2["status"] == "fail", "expected drifted host registry to fail"
        assert "host_registry_checks_drift" in codes2, codes2
        assert "host_local_path_divergence" in codes2, codes2

        # Planted negative 3: repo check missing --keep-going
        _write_valid_fixture(repo_dir, host_dir)
        (repo_dir / ".dsr" / "repos.yaml").write_text(
            (repo_dir / ".dsr" / "repos.yaml")
            .read_text(encoding="utf-8")
            .replace("cargo check --locked --all-targets --keep-going", "cargo check --locked --all-targets"),
            encoding="utf-8",
        )
        res3 = evaluate_contract(repo_dir, None, require_host=False)
        codes3 = {d["code"] for d in res3["diagnostics"]}
        assert res3["status"] == "fail", "expected missing --keep-going to fail"
        assert "missing_keep_going_flag" in codes3, codes3

        # Planted negative 4: missing PI_REQUIRE_GIT_SHA / PI_BUILD_GIT_SHA
        _write_valid_fixture(repo_dir, host_dir)
        (repo_dir / ".dsr" / "repos.d" / "pi_agent_rust.yaml").write_text(
            (repo_dir / ".dsr" / "repos.d" / "pi_agent_rust.yaml")
            .read_text(encoding="utf-8")
            .replace('PI_REQUIRE_GIT_SHA: "1"', 'PI_REQUIRE_GIT_SHA: "0"')
            .replace("PI_BUILD_GIT_SHA", "UNSET_SHA"),
            encoding="utf-8",
        )
        res4 = evaluate_contract(repo_dir, None, require_host=False)
        codes4 = {d["code"] for d in res4["diagnostics"]}
        assert res4["status"] == "fail", "expected missing git SHA provenance enforcement to fail"
        assert "repo_authority_missing_require_git_sha" in codes4, codes4
        assert "repo_authority_build_cmd_missing_git_sha" in codes4, codes4
        assert "repo_authority_win_build_cmd_missing_git_sha" in codes4, codes4

    print("SELF-TEST PASS: check_dsr_contract_sync verified clean + 4 planted drift scenarios")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=Path(__file__).resolve().parent.parent,
        help="Repository root directory",
    )
    parser.add_argument(
        "--host-dsr-dir",
        type=Path,
        default=None,
        help="Override host DSR config directory (default: $DSR_CONFIG_DIR or ~/.config/dsr)",
    )
    parser.add_argument(
        "--check-host",
        action="store_true",
        help="Require ~/.config/dsr/ to exist and match checked-in DSR contract",
    )
    parser.add_argument(
        "--check-host-if-present",
        action="store_true",
        help="Validate ~/.config/dsr/ parity when ~/.config/dsr/ files exist on this host",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Print structured JSON report to stdout",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run built-in positive + planted-negative self-tests",
    )
    args = parser.parse_args()

    if args.self_test:
        return run_self_test()

    require_host = args.check_host or os.environ.get("PI_CHECK_HOST_DSR") == "1"
    check_host_any = require_host or args.check_host_if_present
    host_dir: Path | None = None
    if check_host_any:
        if args.host_dsr_dir is not None:
            host_dir = args.host_dsr_dir
        elif os.environ.get("DSR_CONFIG_DIR"):
            host_dir = Path(os.environ["DSR_CONFIG_DIR"])
        else:
            host_dir = Path.home() / ".config" / "dsr"

    report = evaluate_contract(args.repo_root.resolve(), host_dir, require_host=require_host)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    elif report["status"] == "pass":
        print(
            f"DSR CONTRACT SYNC PASS: {len(report['checked_files'])} file(s) verified, 0 drift findings."
        )
    else:
        print(json.dumps(report, indent=2, sort_keys=True), file=sys.stderr)
    return 0 if report["status"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
