#!/usr/bin/env python3
"""Fail on UBS warning/critical findings that touch staged Rust lines.

`ubs --staged --only=rust` scans whole staged files. For large Rust modules that
already have known baseline findings, that can bury the actionable signal for a
small patch. This gate keeps UBS in the loop while reducing the result to the
lines introduced or modified in the staged diff.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Finding:
    severity: str
    path: str
    line: int
    message: str


@dataclass(frozen=True)
class HookFinding:
    severity: str
    code: str
    detail: str

    def to_json(self) -> dict[str, str]:
        return {
            "severity": self.severity,
            "code": self.code,
            "detail": self.detail,
        }


HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
UBS_LOCATION_RE = re.compile(r"(?P<path>/[^:\n]+?\.rs):(?P<line>\d+)\b")
SUMMARY_RE = re.compile(r"^(Critical|Warning|Info):\s+(\d+)\s*$")
HOOK_SCAN_COMMAND_RE = re.compile(r"(?:^|\s)(?:ubs|[\"']?\$UBS_CMD[\"']?)\s+\.")
EXT_RUNTIME_BASELINE = Path("docs/evidence/ubs-extension-runtime-noise-baseline.json")
EXT_RUNTIME_NOISY_FILES = {"src/extensions.rs", "src/pi_wasm.rs"}
PRECOMMIT_HOOK_AUDIT_SCHEMA = "pi.ubs.pre_commit_hook_contract_audit.v1"
EXPECTED_HOOK_COMMANDS = (
    "ubs --staged --only=rust .",
    "timeout 60s ubs --staged --only=rust .",
    "python3 scripts/check_ubs_staged_delta.py",
)


def run(args: list[str], cwd: Path, *, check: bool = True) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        cwd=cwd,
        check=check,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
    )


def repo_root() -> Path:
    proc = run(["git", "rev-parse", "--show-toplevel"], Path.cwd())
    return Path(proc.stdout.strip()).resolve()


def staged_rust_files(root: Path) -> list[str]:
    proc = run(
        [
            "git",
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--diff-filter=ACMRT",
            "--",
            "*.rs",
        ],
        root,
    )
    return [item for item in proc.stdout.split("\0") if item]


def staged_added_lines(root: Path) -> set[tuple[str, int]]:
    proc = run(
        [
            "git",
            "diff",
            "--cached",
            "--unified=0",
            "--no-ext-diff",
            "--",
            "*.rs",
        ],
        root,
    )

    changed: set[tuple[str, int]] = set()
    current_file: str | None = None
    new_line = 0

    for raw_line in proc.stdout.splitlines():
        if raw_line.startswith("+++ "):
            path = raw_line[4:]
            current_file = path[2:] if path.startswith("b/") else None
            continue

        hunk = HUNK_RE.match(raw_line)
        if hunk:
            new_line = int(hunk.group(1))
            continue

        if current_file is None:
            continue

        if raw_line.startswith("+") and not raw_line.startswith("+++"):
            changed.add((current_file, new_line))
            new_line += 1
        elif raw_line.startswith("-") and not raw_line.startswith("---"):
            continue
        elif raw_line.startswith("\\"):
            continue
        else:
            new_line += 1

    return changed


def normalize_path(root: Path, value: str) -> str | None:
    path = Path(value)
    try:
        return path.resolve().relative_to(root).as_posix()
    except ValueError:
        return None


def parse_ubs_output(root: Path, output: str) -> tuple[list[Finding], dict[str, int]]:
    findings: list[Finding] = []
    totals: dict[str, int] = {}
    severity = ""
    message = ""

    for raw_line in output.splitlines():
        stripped = raw_line.strip()
        summary = SUMMARY_RE.match(stripped)
        if summary:
            totals[summary.group(1).lower()] = int(summary.group(2))
            continue

        if "Critical" in stripped:
            severity = "critical"
            message = ""
            continue
        if "Warning" in stripped:
            severity = "warning"
            message = ""
            continue
        if "Info" in stripped:
            severity = "info"
            message = ""
            continue

        location = UBS_LOCATION_RE.search(raw_line)
        if location and severity:
            rel_path = normalize_path(root, location.group("path"))
            if rel_path is not None:
                # Prefer the description on the SAME line as the location
                # ("file.rs:42:5 – Issue description"). Only fall back to the
                # carried category/context line when the location line has no
                # trailing text, and consume the carried message either way so
                # a previous finding's snippet or fix text never attaches to a
                # later finding in a different file (bd-gel2u class 2).
                remainder = re.sub(r"^:\d+", "", raw_line[location.end() :].strip())
                remainder = remainder.strip().lstrip("–—-:· ").strip()
                findings.append(
                    Finding(
                        severity=severity,
                        path=rel_path,
                        line=int(location.group("line")),
                        message=remainder or message,
                    )
                )
            message = ""
            continue

        # 💡 lines are fix suggestions for the PREVIOUS finding; code-snippet
        # and box-drawing lines are context — none of them may become the
        # message of the NEXT finding.
        if severity and stripped and not stripped.startswith(("✓", "▓", "─", "╔", "║", "╚", "💡")):
            message = stripped

    return findings, totals


def print_runtime_noise_baseline_note(root: Path, staged_files: list[str]) -> None:
    noisy_files = sorted(set(staged_files).intersection(EXT_RUNTIME_NOISY_FILES))
    if not noisy_files:
        return

    baseline_path = root / EXT_RUNTIME_BASELINE
    try:
        baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as err:
        print(
            f"UBS runtime noise baseline unavailable at {EXT_RUNTIME_BASELINE}: {err}",
            file=sys.stderr,
        )
        return

    summary = baseline.get("raw_ubs_summary", {})
    classification_totals = baseline.get("classification_totals", {})
    print(
        "UBS runtime noise baseline: "
        f"{', '.join(noisy_files)} has classified whole-file baseline totals "
        f"critical={summary.get('critical', '?')} "
        f"warning={summary.get('warning', '?')} "
        f"info={summary.get('info', '?')}."
    )
    print(
        "UBS runtime noise baseline: changed-line critical/warning findings still fail; "
        f"classified critical={classification_totals.get('critical_classified', '?')} "
        f"warning={classification_totals.get('warning_classified', '?')} "
        f"({EXT_RUNTIME_BASELINE})."
    )


def hook_command_lines(hook_text: str) -> list[tuple[int, str]]:
    lines: list[tuple[int, str]] = []
    for line_number, raw_line in enumerate(hook_text.splitlines(), start=1):
        stripped = raw_line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        if (
            "--staged" in stripped
            or "--fail-on-warning" in stripped
            or "check_ubs_staged_delta.py" in stripped
            or HOOK_SCAN_COMMAND_RE.search(stripped)
        ):
            lines.append((line_number, stripped))
    return lines


def build_precommit_hook_audit(
    root: Path,
    *,
    hook_exists: bool,
    hook_executable: bool,
    hook_text: str,
) -> dict[str, object]:
    command_lines = hook_command_lines(hook_text)
    uses_staged_ubs = any(
        "--staged" in line and "--only=rust" in line
        for _, line in command_lines
    )
    uses_delta_gate = any(
        "scripts/check_ubs_staged_delta.py" in line
        for _, line in command_lines
    )
    uses_fail_on_warning = any("--fail-on-warning" in line for _, line in command_lines)
    uses_repo_root_scan = any(
        HOOK_SCAN_COMMAND_RE.search(line) and "--staged" not in line
        for _, line in command_lines
    )

    findings: list[HookFinding] = []
    if hook_exists:
        if uses_repo_root_scan:
            findings.append(
                HookFinding(
                    severity="fail",
                    code="repo_wide_ubs_scan",
                    detail=(
                        "pre-commit hook invokes UBS against `.` without `--staged`, "
                        "which can stall or fail on unrelated whole-repo findings"
                    ),
                )
            )
        if uses_fail_on_warning and not uses_staged_ubs:
            findings.append(
                HookFinding(
                    severity="fail",
                    code="repo_wide_fail_on_warning",
                    detail=(
                        "pre-commit hook uses `--fail-on-warning` without the staged-only "
                        "UBS contract"
                    ),
                )
            )
        if not uses_staged_ubs and not uses_delta_gate:
            findings.append(
                HookFinding(
                    severity="fail",
                    code="missing_staged_ubs_gate",
                    detail=(
                        "pre-commit hook does not invoke the staged UBS command or the "
                        "changed-line delta gate"
                    ),
                )
            )

    if any(finding.severity == "fail" for finding in findings):
        status = "action_required"
    elif not hook_exists:
        status = "not_installed"
    else:
        status = "ready"

    hook_path = root / ".git/hooks/pre-commit"
    return {
        "schema": PRECOMMIT_HOOK_AUDIT_SCHEMA,
        "status": status,
        "policy": "read_only_no_hook_mutation",
        "hook_path": hook_path.relative_to(root).as_posix(),
        "hook_exists": hook_exists,
        "hook_executable": hook_executable,
        "expected_commands": list(EXPECTED_HOOK_COMMANDS),
        "observed": {
            "uses_staged_ubs": uses_staged_ubs,
            "uses_delta_gate": uses_delta_gate,
            "uses_fail_on_warning": uses_fail_on_warning,
            "uses_repo_root_scan": uses_repo_root_scan,
            "command_lines": [
                {"line": line_number, "text": line}
                for line_number, line in command_lines
            ],
        },
        "findings": [finding.to_json() for finding in findings],
        "recommended_operator_action": (
            "Update the local pre-commit hook to run `ubs --staged --only=rust .` "
            "or `python3 scripts/check_ubs_staged_delta.py`; do not treat a "
            "repo-wide `ubs . --fail-on-warning` hook as the required staged gate."
        ),
    }


def audit_precommit_hook(root: Path) -> dict[str, object]:
    hook_path = root / ".git/hooks/pre-commit"
    try:
        hook_text = hook_path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return build_precommit_hook_audit(
            root,
            hook_exists=False,
            hook_executable=False,
            hook_text="",
        )
    except OSError as err:
        return {
            "schema": PRECOMMIT_HOOK_AUDIT_SCHEMA,
            "status": "action_required",
            "policy": "read_only_no_hook_mutation",
            "hook_path": hook_path.relative_to(root).as_posix(),
            "hook_exists": True,
            "hook_executable": hook_path.is_file() and os.access(hook_path, os.X_OK),
            "expected_commands": list(EXPECTED_HOOK_COMMANDS),
            "observed": {},
            "findings": [
                HookFinding(
                    severity="fail",
                    code="hook_unreadable",
                    detail=f"failed to read pre-commit hook: {err}",
                ).to_json()
            ],
            "recommended_operator_action": (
                "Inspect the local pre-commit hook permissions and rerun this audit."
            ),
        }

    return build_precommit_hook_audit(
        root,
        hook_exists=True,
        hook_executable=os.access(hook_path, os.X_OK),
        hook_text=hook_text,
    )


def print_precommit_hook_audit(report: dict[str, object]) -> None:
    print(f"UBS pre-commit hook audit: {report['status']}")
    print(f"  hook: {report['hook_path']}")
    observed = report.get("observed", {})
    if isinstance(observed, dict):
        print(
            "  observed: "
            f"staged_ubs={observed.get('uses_staged_ubs')} "
            f"delta_gate={observed.get('uses_delta_gate')} "
            f"repo_root_scan={observed.get('uses_repo_root_scan')} "
            f"fail_on_warning={observed.get('uses_fail_on_warning')}"
        )
    for finding in report.get("findings", []):
        if isinstance(finding, dict):
            print(
                f"  {finding.get('severity')}: "
                f"{finding.get('code')} - {finding.get('detail')}"
            )
    print(f"  next: {report['recommended_operator_action']}")


def run_precommit_hook_self_test() -> int:
    root = Path("/repo")

    missing = build_precommit_hook_audit(
        root,
        hook_exists=False,
        hook_executable=False,
        hook_text="",
    )
    assert missing["status"] == "not_installed"

    staged = build_precommit_hook_audit(
        root,
        hook_exists=True,
        hook_executable=True,
        hook_text="#!/bin/sh\nubs --staged --only=rust .\n",
    )
    assert staged["status"] == "ready"
    assert not staged["findings"]

    delta = build_precommit_hook_audit(
        root,
        hook_exists=True,
        hook_executable=True,
        hook_text="#!/bin/sh\npython3 scripts/check_ubs_staged_delta.py\n",
    )
    assert delta["status"] == "ready"
    assert not delta["findings"]

    repo_wide = build_precommit_hook_audit(
        root,
        hook_exists=True,
        hook_executable=True,
        hook_text='#!/bin/sh\nif ! "$UBS_CMD" . --fail-on-warning; then exit 1; fi\n',
    )
    assert repo_wide["status"] == "action_required"
    codes = {
        finding["code"]
        for finding in repo_wide["findings"]
        if isinstance(finding, dict)
    }
    assert "repo_wide_ubs_scan" in codes
    assert "repo_wide_fail_on_warning" in codes
    assert "missing_staged_ubs_gate" in codes

    mixed = build_precommit_hook_audit(
        root,
        hook_exists=True,
        hook_executable=True,
        hook_text=(
            "#!/bin/sh\n"
            "ubs --staged --only=rust .\n"
            'if ! "$UBS_CMD" . --fail-on-warning; then exit 1; fi\n'
        ),
    )
    assert mixed["status"] == "action_required"
    mixed_codes = {
        finding["code"]
        for finding in mixed["findings"]
        if isinstance(finding, dict)
    }
    assert "repo_wide_ubs_scan" in mixed_codes
    assert "missing_staged_ubs_gate" not in mixed_codes

    print("PRE-COMMIT HOOK SELF-TEST PASS")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run UBS on staged Rust files and fail on changed-line warnings/critical findings.",
    )
    parser.add_argument(
        "--check-pre-commit-hook",
        action="store_true",
        help="Read-only audit of .git/hooks/pre-commit against the staged UBS contract.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit JSON for --check-pre-commit-hook.",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run in-memory checks for the pre-commit hook audit mode.",
    )
    parser.add_argument(
        "--fail-on-info",
        action="store_true",
        help="Also fail when UBS info findings land on staged changed lines.",
    )
    parser.add_argument(
        "--print-ubs-output",
        action="store_true",
        help="Print raw UBS output after the delta summary.",
    )
    args = parser.parse_args()

    root = repo_root()
    if args.self_test:
        return run_precommit_hook_self_test()

    if args.check_pre_commit_hook:
        report = audit_precommit_hook(root)
        if args.json:
            print(json.dumps(report, indent=2, sort_keys=True))
        else:
            print_precommit_hook_audit(report)
        return 1 if report["status"] == "action_required" else 0

    files = staged_rust_files(root)
    if not files:
        print("UBS staged delta: no staged Rust files; skipping.")
        return 0

    changed = staged_added_lines(root)
    if not changed:
        print("UBS staged delta: staged Rust files have no added/modified lines; skipping.")
        return 0

    ubs = shutil.which("ubs")
    if ubs is None:
        print("UBS staged delta: 'ubs' command not found.", file=sys.stderr)
        return 127

    env = os.environ.copy()
    env["UBS_OUTPUT_FORMAT"] = "text"
    proc = subprocess.run(
        [ubs, "--staged", "--only=rust", "--format=text", "--ci", "."],
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
    )
    findings, totals = parse_ubs_output(root, proc.stdout)

    changed_findings = [
        finding
        for finding in findings
        if (finding.path, finding.line) in changed
    ]
    fail_severities = {"critical", "warning"}
    if args.fail_on_info:
        fail_severities.add("info")
    failures = [finding for finding in changed_findings if finding.severity in fail_severities]

    print(
        "UBS staged delta: "
        f"{len(files)} staged Rust file(s), "
        f"{len(changed)} changed line(s), "
        f"{len(findings)} parsed UBS location(s), "
        f"{len(changed_findings)} finding location(s) on changed lines."
    )
    if totals:
        print(
            "UBS staged totals: "
            f"critical={totals.get('critical', 0)} "
            f"warning={totals.get('warning', 0)} "
            f"info={totals.get('info', 0)}"
        )

    if proc.returncode != 0 and not findings and not totals:
        print(f"UBS command failed with exit {proc.returncode}.", file=sys.stderr)
        print(proc.stdout[-4000:], file=sys.stderr)
        return proc.returncode

    if failures:
        print("UBS staged delta failed on changed-line findings:", file=sys.stderr)
        for finding in failures:
            detail = f" - {finding.severity}: {finding.path}:{finding.line}"
            if finding.message:
                detail += f" - {finding.message}"
            print(detail, file=sys.stderr)
        if args.print_ubs_output:
            print(proc.stdout)
        return 1

    if changed_findings:
        print("UBS staged delta info-only findings on changed lines:")
        for finding in changed_findings:
            detail = f" - {finding.severity}: {finding.path}:{finding.line}"
            if finding.message:
                detail += f" - {finding.message}"
            print(detail)

    if proc.returncode != 0:
        print(
            "UBS command returned nonzero for whole-file findings; "
            "continuing because no warning/critical finding lands on a staged changed line."
        )

    print_runtime_noise_baseline_note(root, files)
    print("UBS staged delta passed: no warning/critical findings on staged changed lines.")
    if args.print_ubs_output:
        print(proc.stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
