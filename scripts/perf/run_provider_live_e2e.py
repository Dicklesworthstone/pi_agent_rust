#!/usr/bin/env python3
"""scripts/perf/run_provider_live_e2e.py

Per-provider live E2E validation harness (bd-provider-live-validation-11).

For each of the 11 native providers, spawns `target/release/pi` in
single-shot mode against a fixture prompt, captures stdout/stderr, and
records per-provider pass / skip-with-reason / fail with measured
latency.

Skips providers whose env-var credentials are not set. Documents
the skip reason in the runpack so reviewers know what was and was
not exercised.

Exit 0 = harness ran (pass+skip+fail status captured).
Exit 1 = harness setup error (binary missing, no providers enumerated).
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

# Be robust against any cwd: derive ROOT from __file__ when available,
# else from the script's known location.
SCRIPT = Path(__file__).resolve()
ROOT = SCRIPT.parents[2]  # scripts/perf/ → scripts/ → <repo>

# (provider_id, env_var_for_creds, fixture_prompt)
PROVIDERS = [
    ("openai-codex", "OPENAI_CODEX_AUTH_FILE", "What is 2+2? Answer in one word."),
    ("anthropic", "ANTHROPIC_API_KEY", "What is 2+2? Answer in one word."),
    ("openai", "OPENAI_API_KEY", "What is 2+2? Answer in one word."),
    ("openai_responses", "OPENAI_API_KEY", "What is 2+2? Answer in one word."),
    ("gemini", "GOOGLE_API_KEY", "What is 2+2? Answer in one word."),
    ("cohere", "COHERE_API_KEY", "What is 2+2? Answer in one word."),
    ("azure", "AZURE_OPENAI_API_KEY", "What is 2+2? Answer in one word."),
    ("bedrock", "AWS_ACCESS_KEY_ID", "What is 2+2? Answer in one word."),
    ("vertex", "GOOGLE_APPLICATION_CREDENTIALS", "What is 2+2? Answer in one word."),
    ("copilot", "GITHUB_COPILOT_TOKEN", "What is 2+2? Answer in one word."),
    ("gitlab", "GITLAB_TOKEN", "What is 2+2? Answer in one word."),
    ("cursor", "CURSOR_API_KEY", "What is 2+2? Answer in one word."),
]


def has_credentials(prov_id: str, env_var: str) -> tuple[bool, str]:
    """Return whether credentials appear available for `prov_id` without printing secrets."""
    if os.environ.get(env_var):
        return True, f"env:{env_var}"
    if prov_id == "openai-codex":
        for candidate in (
            Path.home() / ".codex" / "auth.json",
            Path.home() / ".pi" / "agent" / "auth.json",
        ):
            if candidate.is_file():
                try:
                    data = json.loads(candidate.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError):
                    continue
                if isinstance(data, dict) and (
                    "tokens" in data or "openai-codex" in data or "OPENAI_API_KEY" in data
                ):
                    return True, f"file:{candidate.name}"
    return False, f"{env_var} not set"


def parse_default_probe_response(stdout: str, stderr: str, returncode: int) -> dict[str, object]:
    """Extract selected model, HTTP status, and error code from `--mode json` output."""
    selected_model: str | None = None
    error_code: str | None = None
    http_status: int | None = None

    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        msg = event.get("message")
        if isinstance(msg, dict) and isinstance(msg.get("model"), str):
            selected_model = msg["model"]
        if isinstance(event.get("model"), str):
            selected_model = event["model"]
        if event.get("type") == "error":
            if isinstance(event.get("code"), str):
                error_code = event["code"]

    combined = f"{stdout}\n{stderr}"
    if "model_not_found" in combined:
        error_code = "model_not_found"
    if "HTTP 400" in combined:
        http_status = 400
    elif "HTTP 401" in combined:
        http_status = 401
    elif "HTTP 403" in combined:
        http_status = 403
    elif "HTTP 404" in combined:
        http_status = 404
    elif "HTTP 429" in combined:
        http_status = 429
    elif returncode == 0:
        http_status = 200

    return {
        "selected_model": selected_model,
        "http_status": http_status,
        "error_code": error_code,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", type=Path, default=ROOT / "target/release/pi")
    ap.add_argument("--prompt", default="What is 2+2? Answer in one word.")
    ap.add_argument("--timeout", type=float, default=30.0)
    ap.add_argument(
        "--check-defaults",
        action="store_true",
        help=(
            "Probe each credentialed provider WITHOUT --model using --mode json "
            "and write structured JSONL (pi.provider.default_probe.v1) verifying "
            "auto-selected default models do not fail with HTTP 400 model_not_found."
        ),
    )
    ap.add_argument(
        "--out", type=Path,
        default=ROOT / "docs/evidence/provider-live-validation-runpack.json",
    )
    ap.add_argument(
        "--defaults-jsonl",
        type=Path,
        default=ROOT / "docs/evidence/provider-default-probe.jsonl",
    )
    args = ap.parse_args()

    args.out.parent.mkdir(parents=True, exist_ok=True)
    if args.check_defaults:
        args.defaults_jsonl.parent.mkdir(parents=True, exist_ok=True)

    if not args.binary.exists():
        runpack = {
            "schema": "pi.perf.provider_live_e2e.v1",
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "verdict": "binary_missing",
            "providers": [
                {"id": p, "status": "skipped_no_binary", "reason": "target/release/pi not built"}
                for p, _, _ in PROVIDERS
            ],
        }
        with open(args.out, "w") as f:
            json.dump(runpack, f, indent=2)
        print(f"FAIL: binary not found at {args.binary}", file=sys.stderr)
        print(f"  wrote {args.out} with verdict=binary_missing; rerun after `cargo build --release --bin pi`")
        return 1

    results = []
    default_probe_rows: list[dict[str, object]] = []
    pass_count = 0
    fail_count = 0
    skip_count = 0

    for prov_id, env_var, _ in PROVIDERS:
        creds_ok, creds_reason = has_credentials(prov_id, env_var)
        if not creds_ok:
            results.append({
                "id": prov_id, "status": "skipped_no_credentials",
                "reason": creds_reason,
            })
            if args.check_defaults:
                default_probe_rows.append({
                    "schema": "pi.provider.default_probe.v1",
                    "provider": prov_id,
                    "selected_model": None,
                    "http_status": None,
                    "latency_ms": 0.0,
                    "verdict": "skipped_no_credentials",
                    "error_code": None,
                })
            skip_count += 1
            continue

        cmd = [str(args.binary), "--no-session", "--provider", prov_id, "--print"]
        if args.check_defaults:
            cmd.extend(["--mode", "json"])
        cmd.append(args.prompt)
        env = os.environ.copy()
        env["PI_PROVIDER"] = prov_id
        t0 = time.monotonic()
        try:
            proc = subprocess.run(
                cmd, capture_output=True, text=True, timeout=args.timeout, env=env,
            )
            elapsed = round((time.monotonic() - t0) * 1000, 1)
            ok = proc.returncode == 0 and bool(proc.stdout.strip())
            parsed = parse_default_probe_response(proc.stdout, proc.stderr, proc.returncode)
            results.append({
                "id": prov_id, "status": "pass" if ok else "fail",
                "elapsed_ms": elapsed,
                "exit_code": proc.returncode,
                "selected_model": parsed["selected_model"],
                "http_status": parsed["http_status"],
                "error_code": parsed["error_code"],
                "stdout_head": proc.stdout[:200],
                "stderr_head": proc.stderr[:200],
            })
            if args.check_defaults:
                default_probe_rows.append({
                    "schema": "pi.provider.default_probe.v1",
                    "provider": prov_id,
                    "selected_model": parsed["selected_model"],
                    "http_status": parsed["http_status"],
                    "latency_ms": elapsed,
                    "verdict": "pass" if ok else "fail",
                    "error_code": parsed["error_code"],
                })
            if ok:
                pass_count += 1
            else:
                fail_count += 1
        except subprocess.TimeoutExpired:
            elapsed = round((time.monotonic() - t0) * 1000, 1)
            results.append({
                "id": prov_id, "status": "fail",
                "reason": f"timeout after {args.timeout}s",
            })
            if args.check_defaults:
                default_probe_rows.append({
                    "schema": "pi.provider.default_probe.v1",
                    "provider": prov_id,
                    "selected_model": None,
                    "http_status": None,
                    "latency_ms": elapsed,
                    "verdict": "fail",
                    "error_code": "timeout",
                })
            fail_count += 1
        except Exception as e:
            elapsed = round((time.monotonic() - t0) * 1000, 1)
            results.append({
                "id": prov_id, "status": "fail",
                "reason": f"exception: {e}",
            })
            if args.check_defaults:
                default_probe_rows.append({
                    "schema": "pi.provider.default_probe.v1",
                    "provider": prov_id,
                    "selected_model": None,
                    "http_status": None,
                    "latency_ms": elapsed,
                    "verdict": "fail",
                    "error_code": f"exception:{type(e).__name__}",
                })
            fail_count += 1

    if args.check_defaults:
        with open(args.defaults_jsonl, "w", encoding="utf-8") as f:
            for row in default_probe_rows:
                f.write(json.dumps(row, sort_keys=True) + "\n")
        print(f"wrote {args.defaults_jsonl} ({len(default_probe_rows)} rows)")

    runpack = {
        "schema": "pi.perf.provider_live_e2e.v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "verdict": "complete" if skip_count == 0 else "partial",
        "pass_count": pass_count,
        "fail_count": fail_count,
        "skip_count": skip_count,
        "providers": results,
    }

    with open(args.out, "w") as f:
        json.dump(runpack, f, indent=2)

    print(f"wrote {args.out}: pass={pass_count} fail={fail_count} skip={skip_count}")
    return 1 if (args.check_defaults and fail_count > 0) else 0


if __name__ == "__main__":
    sys.exit(main())
