#!/usr/bin/env python3
"""scripts/perf/run_provider_live_e2e.py

Per-provider live E2E validation harness (bd-provider-live-validation-11).

For each of the 11 native providers, spawns `target/release/pi` in
single-shot mode against a fixture prompt, captures stdout/stderr, and
records per-provider pass / skip-with-reason / fail with measured
latency.

Skips providers without a detected credential source. Documents
the skip reason in the runpack so reviewers know what was and was
not exercised.

Exit 0 = harness ran (pass+skip+fail status captured).
Exit 1 = harness setup error (binary missing, no providers enumerated).
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path

# Be robust against any cwd: derive ROOT from __file__ when available,
# else from the script's known location.
SCRIPT = Path(__file__).resolve()
ROOT = SCRIPT.parents[2]  # scripts/perf/ → scripts/ → <repo>

# Runpack IDs stay stable. The command always uses the canonical provider ID.
PROVIDERS = {
    "openai-codex": "openai-codex",
    "anthropic": "anthropic",
    "openai": "openai",
    "gemini": "google",
    "cohere": "cohere",
    "azure": "azure-openai",
    "bedrock": "amazon-bedrock",
    "vertex": "google-vertex",
    "copilot": "github-copilot",
    "gitlab": "gitlab",
    "cursor": "cursor",
}

HTTP_ERROR_RE = re.compile(r"\bHTTP\s+([45]\d\d)\b")


def provider_inventory(binary: Path, timeout: float) -> tuple[dict, set[str]]:
    """Ask the same pi binary which providers and credential-backed models it knows."""
    def query(*args: str) -> str:
        return subprocess.run(
            [str(binary), *args], capture_output=True, text=True, check=True, timeout=timeout
        ).stdout

    rows = json.loads(query("--list-providers", "--mode", "json"))["providers"]
    metadata = {row["id"]: row for row in rows}
    missing = set(PROVIDERS.values()) - metadata.keys()
    if missing:
        raise ValueError("unregistered provider selectors: " + ", ".join(sorted(missing)))
    # --list-models uses the registry's actual credential resolver, including
    # stored OAuth, external logins, configured keys and custom models.json.
    listed = {parts[0] for line in query("--list-models").splitlines() if (parts := line.split())}
    return metadata, listed


def bedrock_credentials_configured(names: set[str]) -> bool:
    # Bedrock is admitted by --list-models even without any AWS credentials.
    # Region/session token alone does not constitute a usable credential source.
    if any(os.environ.get(key, "").strip() for key in (
        "AWS_BEARER_TOKEN_BEDROCK", "AWS_PROFILE", "AWS_DEFAULT_PROFILE"
    )):
        return True
    if all(os.environ.get(key, "").strip() for key in ("AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY")):
        return True
    agent_dir = Path(os.environ.get("PI_CODING_AGENT_DIR", Path.home() / ".pi" / "agent"))
    try:
        auth = json.loads((agent_dir / "auth.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return False
    if not isinstance(auth, dict):
        return False
    for provider, credential in auth.items():
        if provider.lower() not in names or not isinstance(credential, dict):
            continue
        kind = credential.get("type")
        if kind == "aws_credentials":
            fields = ("access_key_id", "secret_access_key")
        elif kind == "bearer_token":
            fields = ("token",)
        elif kind == "api_key":
            fields = ("key",)
        else:
            continue
        if all(isinstance(credential.get(field), str) and credential[field].strip() for field in fields):
            return True
    return False


def has_credentials(metadata: dict, listed: set[str], names: set[str]) -> bool:
    if metadata["id"] == "amazon-bedrock":
        return bedrock_credentials_configured(names)
    return bool(listed & names) or any(
        os.environ.get(key, "").strip() for key in metadata["env_keys"]
    )


def parse_default_probe_response(
    stdout: str, stderr: str, returncode: int, *, check_defaults: bool = True,
    provider_names: set[str] | None = None,
) -> dict[str, object]:
    """Interpret the print stream without treating progress as a completed provider call."""
    selected_model: str | None = None
    failure = "missing_assistant_completion" if check_defaults else None
    diagnostic = stderr if check_defaults else f"{stdout}\n{stderr}"
    failed_over = False

    json_lines = stdout.splitlines() if check_defaults else ()
    for line in json_lines:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(event, dict):
            continue
        kind = event.get("type")
        if kind == "failover_start":
            # A successful fallback does not validate the default destination.
            if not failed_over and isinstance(event.get("fromModel"), str):
                selected_model = event["fromModel"]
            failed_over = True
        if failed_over:
            continue
        if kind == "error":
            code = event.get("code")
            failure = code if isinstance(code, str) and code else "provider_error"
            diagnostic = f"{stderr}\n{event.get('message', '')}"
            continue
        msg = event.get("message")
        if not isinstance(msg, dict) or msg.get("role") != "assistant":
            continue
        if kind not in ("message_start", "message_end"):
            continue
        model = msg.get("model")
        selected_model = model if isinstance(model, str) and model.strip() else None
        diagnostic = stderr
        failure = "missing_assistant_completion"
        if kind == "message_end":
            reason = msg.get("stopReason")
            content = msg.get("content")
            if msg.get("errorMessage") or reason in ("error", "aborted", "refusal"):
                failure = reason if reason in ("error", "aborted", "refusal") else "provider_error"
                diagnostic = f"{stderr}\n{msg.get('errorMessage', '')}"
            elif provider_names is not None and str(msg.get("provider", "")).lower() not in provider_names:
                failure = "unexpected_provider"
            elif selected_model and reason in ("stop", "length") and isinstance(content, list):
                if any(
                    isinstance(block, dict) and block.get("type") == "text"
                    and isinstance(block.get("text"), str) and block["text"].strip()
                    for block in content
                ):
                    failure = None

    if failed_over:
        failure = "default_model_failover"
    if returncode != 0 and failure in (None, "missing_assistant_completion"):
        failure = "process_failed"
    http_status: int | None = None
    if failure is None:
        http_status = 200
    else:
        if "model_not_found" in diagnostic:
            failure = "model_not_found"
        match = HTTP_ERROR_RE.search(diagnostic)
        if match:
            http_status = int(match.group(1))

    return {
        "selected_model": selected_model,
        "http_status": http_status,
        "error_code": failure,
    }


def self_test() -> int:
    assistant = {
        "role": "assistant", "model": "gpt-5.6-sol", "stopReason": "stop",
        "content": [{"type": "text", "text": "HTTP 400 means bad request"}],
    }
    end = json.dumps({"type": "message_end", "message": assistant})
    start = json.dumps({"type": "message_start", "message": assistant})
    ok_resp = parse_default_probe_response(start + "\n" + end, "", 0)
    assert ok_resp["http_status"] == 200, ok_resp
    assert ok_resp["selected_model"] == "gpt-5.6-sol", ok_resp
    assert ok_resp["error_code"] is None, ok_resp

    for incomplete in (
        "progress only", start, end + "\n" + start,
        json.dumps({"type": "message_end", "message": dict(assistant, role="user")}),
        json.dumps({"type": "message_end", "message": dict(assistant, content=[])}),
        json.dumps({"type": "message_end", "message": dict(assistant, model="")}),
    ):
        parsed = parse_default_probe_response(incomplete, "", 0)
        assert parsed["error_code"] == "missing_assistant_completion", parsed
        assert parsed["http_status"] is None, parsed

    errored = dict(assistant, stopReason="error", errorMessage="HTTP 400: model_not_found")
    error_end = json.dumps({"type": "message_end", "message": errored})
    parsed = parse_default_probe_response(error_end, "", 0)
    assert (parsed["http_status"], parsed["error_code"]) == (400, "model_not_found"), parsed
    assert parse_default_probe_response(error_end + "\n" + end, "", 0)["error_code"] is None
    swap = json.dumps({"type": "failover_start", "fromModel": "failed-default"})
    parsed = parse_default_probe_response(swap + "\n" + end, "", 0)
    assert parsed["error_code"] == "default_model_failover", parsed
    assert parsed["selected_model"] == "failed-default", parsed
    assert parse_default_probe_response("four", "", 0, check_defaults=False)["error_code"] is None
    assert parse_default_probe_response(end, "", 1)["error_code"] == "process_failed"

    err400 = parse_default_probe_response(
        "", "HTTP 400 Bad Request: model_not_found", 1
    )
    assert err400["http_status"] == 400, err400
    assert err400["error_code"] == "model_not_found", err400

    err503 = parse_default_probe_response("", "HTTP 503 Service Unavailable", 1)
    assert err503["http_status"] == 503, err503

    wrong_provider = dict(assistant, provider="openai")
    record = json.dumps({"type": "message_end", "message": wrong_provider})
    assert parse_default_probe_response(record, "", 0, provider_names={"google", "gemini"})["error_code"] == "unexpected_provider"
    assert parse_default_probe_response(record, "", 0, provider_names={"openai"})["error_code"] is None

    # Model admission includes OAuth-only providers. AWS needs its own check:
    # the catalog admits Bedrock even when the only AWS variable is a region.
    from unittest.mock import patch

    with tempfile.TemporaryDirectory(prefix="pi-live-probe-test-") as tmp:
        with patch.dict(os.environ, {"PI_CODING_AGENT_DIR": tmp, "AWS_REGION": "us-west-2"}, clear=True):
            names = {"amazon-bedrock", "bedrock"}
            bedrock = {"id": "amazon-bedrock", "env_keys": ["AWS_REGION"]}
            assert not has_credentials(bedrock, names, names)
            os.environ["AWS_ACCESS_KEY_ID"] = "fixture"
            assert not has_credentials(bedrock, names, names)
            os.environ["AWS_SECRET_ACCESS_KEY"] = "fixture"
            assert has_credentials(bedrock, set(), names)
            del os.environ["AWS_SECRET_ACCESS_KEY"]
            Path(tmp, "auth.json").write_text(json.dumps({
                "bedrock": {"type": "bearer_token", "token": "fixture"}
            }))
            assert has_credentials(bedrock, set(), names)
            codex = {"id": "openai-codex", "env_keys": []}
            assert not has_credentials(codex, {"openai"}, {"openai-codex"})
            assert has_credentials(codex, {"openai-codex"}, {"openai-codex"})
            copilot = {"id": "github-copilot", "env_keys": ["GITHUB_TOKEN"]}
            os.environ["GITHUB_TOKEN"] = "fixture"
            assert has_credentials(copilot, set(), {"github-copilot"})

    print("SELF-TEST PASS")
    return 0


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
    ap.add_argument("--self-test", action="store_true", help="Run internal self-tests and exit")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

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
                for p in PROVIDERS
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

    try:
        catalog, listed = provider_inventory(args.binary, args.timeout)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(f"FAIL: cannot enumerate providers with {args.binary}: {error}", file=sys.stderr)
        return 1

    for prov_id, selector in PROVIDERS.items():
        metadata = catalog[selector]
        names = {selector, *metadata["aliases"]}
        if not has_credentials(metadata, listed, names):
            results.append({
                "id": prov_id, "status": "skipped_no_credentials",
                "reason": f"no configured credentials detected for {selector}",
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

        cmd = [str(args.binary), "--no-session", "--provider", selector, "--print"]
        if args.check_defaults:
            cmd.extend(["--mode", "json"])
        cmd.append(args.prompt)
        env = os.environ.copy()
        env["PI_PROVIDER"] = selector
        if args.check_defaults:
            # Clap treats PI_MODEL exactly like an explicit --model override.
            env.pop("PI_MODEL", None)
        t0 = time.monotonic()
        try:
            proc = subprocess.run(
                cmd, capture_output=True, text=True, timeout=args.timeout, env=env,
            )
            elapsed = round((time.monotonic() - t0) * 1000, 1)
            parsed = parse_default_probe_response(
                proc.stdout, proc.stderr, proc.returncode,
                check_defaults=args.check_defaults, provider_names=names,
            )
            ok = proc.returncode == 0 and bool(proc.stdout.strip())
            if args.check_defaults:
                ok = ok and parsed["error_code"] is None
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
