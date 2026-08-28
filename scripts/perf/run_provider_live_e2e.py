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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", type=Path, default=ROOT / "target/release/pi")
    ap.add_argument("--prompt", default="What is 2+2? Answer in one word.")
    ap.add_argument("--timeout", type=float, default=30.0)
    ap.add_argument(
        "--out", type=Path,
        default=ROOT / "docs/evidence/provider-live-validation-runpack.json",
    )
    args = ap.parse_args()

    args.out.parent.mkdir(parents=True, exist_ok=True)

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
    pass_count = 0
    fail_count = 0
    skip_count = 0

    for prov_id, env_var, _ in PROVIDERS:
        env_val = os.environ.get(env_var)
        if not env_val:
            results.append({
                "id": prov_id, "status": "skipped_no_credentials",
                "reason": f"{env_var} not set",
            })
            skip_count += 1
            continue

        cmd = [str(args.binary), "--provider", prov_id, "--print", args.prompt]
        env = os.environ.copy()
        env["PI_PROVIDER"] = prov_id
        t0 = time.monotonic()
        try:
            proc = subprocess.run(
                cmd, capture_output=True, text=True, timeout=args.timeout, env=env,
            )
            elapsed = (time.monotonic() - t0) * 1000
            ok = proc.returncode == 0 and proc.stdout.strip()
            results.append({
                "id": prov_id, "status": "pass" if ok else "fail",
                "elapsed_ms": round(elapsed, 1),
                "exit_code": proc.returncode,
                "stdout_head": proc.stdout[:200],
                "stderr_head": proc.stderr[:200],
            })
            if ok:
                pass_count += 1
            else:
                fail_count += 1
        except subprocess.TimeoutExpired:
            results.append({
                "id": prov_id, "status": "fail",
                "reason": f"timeout after {args.timeout}s",
            })
            fail_count += 1
        except Exception as e:
            results.append({
                "id": prov_id, "status": "fail",
                "reason": f"exception: {e}",
            })
            fail_count += 1

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
    return 0


if __name__ == "__main__":
    sys.exit(main())
