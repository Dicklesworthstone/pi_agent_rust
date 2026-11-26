#!/usr/bin/env python3
"""scripts/perf/measure_idle_memory.py

Canonical idle-memory measurement control
(`src/perf_build.rs::IdleRssMeasurementControl`, `#[serde(deny_unknown_fields)]`).

Spawns `sample_count >= 5` independent release `pi --mode rpc --no-session`
processes (stdin held open so each process initializes and rests genuinely idle
at `startup_before_user_input` without contacting any LLM provider), samples
each unique PID's VmRSS after a bounded settle window (`100..=10000 ms`),
aggregates the worst-case maximum RSS and exact max-minus-min spread, and
writes the exact 22-field `pi.perf.idle_rss_measurement.v1` control artifact
verified by `src/perf_build.rs::verify_idle_rss_measurement_control`.

Exit 0 = artifact written, schema valid, sample_count >= 5 unique PIDs.
Exit 1 = setup error or binary missing.
Exit 2 = process exited prematurely before RSS sampling.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SCHEMA = "pi.perf.idle_rss_measurement.v1"
DEFAULT_SETTLE_MS = 1000
DEFAULT_SAMPLE_COUNT = 5
REQUIRED_FIELDS = (
    "schema",
    "generated_at",
    "run_id",
    "correlation_id",
    "source_commit",
    "source_dirty",
    "pid",
    "process_name",
    "allocator",
    "binary_path",
    "binary_sha256",
    "rss_bytes",
    "idle_state",
    "cargo_profile",
    "build_command",
    "sample_count",
    "samples",
    "rss_spread_bytes",
    "settle_ms",
    "bench_env_source",
    "bench_env",
    "bench_env_sha256",
)
BENCH_ENV_FIELDS = (
    "os",
    "arch",
    "cpu_brand",
    "cpu_cores",
    "mem_total_mb",
    "governor",
    "turbo_boost",
    "aslr",
    "thp",
    "noise_score",
    "config_hash",
)


def project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def git_source_identity(workdir: Path) -> tuple[str, bool]:
    commit = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=workdir, text=True
    ).strip()
    dirty = bool(
        subprocess.check_output(
            ["git", "status", "--porcelain"], cwd=workdir, text=True
        ).strip()
    )
    return commit, dirty


def read_text_opt(path: str) -> str | None:
    try:
        return Path(path).read_text(encoding="utf-8").strip()
    except OSError:
        return None


def rss_linux(pid: int) -> int | None:
    text = read_text_opt(f"/proc/{pid}/status")
    if text is None:
        return None
    for line in text.splitlines():
        if line.startswith("VmRSS:"):
            parts = line.split()
            if len(parts) >= 2:
                return int(parts[1]) * 1024
    return None


def rss_macos(pid: int) -> int | None:
    try:
        out = subprocess.check_output(
            ["ps", "-o", "rss=", "-p", str(pid)], text=True, timeout=2.0
        ).strip()
        return int(out) * 1024
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, ValueError):
        return None


def sample_pid_rss(pid: int) -> int | None:
    if platform.system() == "Darwin":
        return rss_macos(pid)
    return rss_linux(pid)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def collect_bench_env(source_commit: str) -> tuple[dict[str, Any], str]:
    """Collect the 11-field BenchEnvMeasurementControl matching benches/bench_env.rs."""
    governor = read_text_opt("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor") or "unavailable"

    turbo = "unavailable"
    intel_no_turbo = read_text_opt("/sys/devices/system/cpu/intel_pstate/no_turbo")
    if intel_no_turbo is not None:
        turbo = "disabled" if intel_no_turbo == "1" else "enabled"
    else:
        amd_boost = read_text_opt("/sys/devices/system/cpu/cpufreq/boost")
        if amd_boost is not None:
            turbo = "disabled" if amd_boost == "0" else "enabled"

    aslr_raw = read_text_opt("/proc/sys/kernel/randomize_va_space")
    aslr = {"0": "disabled", "1": "partial", "2": "full"}.get(
        aslr_raw or "", f"unknown({aslr_raw})" if aslr_raw else "unavailable"
    )

    thp = "unavailable"
    thp_raw = read_text_opt("/sys/kernel/mm/transparent_hugepage/enabled")
    if thp_raw:
        for token in thp_raw.split():
            if token.startswith("[") and token.endswith("]"):
                thp = token[1:-1]
                break

    noise_score = 0
    if governor not in ("performance", "unavailable"):
        noise_score += 3
    if turbo == "enabled":
        noise_score += 2
    if thp not in ("never", "unavailable"):
        noise_score += 1
    if aslr not in ("disabled", "unavailable"):
        noise_score += 1

    cpu_brand = platform.processor() or "unknown"
    cpuinfo = read_text_opt("/proc/cpuinfo")
    if cpuinfo:
        for line in cpuinfo.splitlines():
            if line.lower().startswith("model name"):
                _, _, rhs = line.partition(":")
                if rhs.strip():
                    cpu_brand = rhs.strip()
                    break

    mem_total_mb = 0
    meminfo = read_text_opt("/proc/meminfo")
    if meminfo:
        for line in meminfo.splitlines():
            if line.startswith("MemTotal:"):
                mem_total_mb = int(line.split()[1]) // 1024
                break
    if mem_total_mb == 0:
        try:
            mem_total_mb = (os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")) // (1024 * 1024)
        except (ValueError, OSError, AttributeError):
            mem_total_mb = 1

    arch_raw = platform.machine().lower()
    arch = {"amd64": "x86_64", "arm64": "aarch64"}.get(arch_raw, arch_raw or "unknown")
    os_desc = f"{platform.system()} {platform.release()}".strip() or "unknown"
    config_hash = hashlib.sha256(f"git_sha={source_commit}".encode("utf-8")).hexdigest()

    bench_env = {
        "os": os_desc,
        "arch": arch,
        "cpu_brand": cpu_brand,
        "cpu_cores": max(1, os.cpu_count() or 1),
        "mem_total_mb": max(1, mem_total_mb),
        "governor": governor,
        "turbo_boost": turbo,
        "aslr": aslr,
        "thp": thp,
        "noise_score": noise_score,
        "config_hash": config_hash,
    }
    assert tuple(sorted(bench_env.keys())) == tuple(sorted(BENCH_ENV_FIELDS))
    # Matches serde_json::to_value + serde_json::to_vec (BTreeMap sorted keys, compact UTF-8 JSON).
    canonical = json.dumps(
        bench_env, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")
    bench_env_sha256 = hashlib.sha256(canonical).hexdigest()
    return bench_env, bench_env_sha256


def measure_one_idle_process(
    binary: Path,
    *,
    settle_ms: int,
    agent_dir: Path,
) -> tuple[dict[str, Any] | None, str | None]:
    env = os.environ.copy()
    env.pop("RUST_LOG", None)
    env["PI_CODING_AGENT_DIR"] = str(agent_dir)
    cmd = [str(binary), "--mode", "rpc", "--no-session"]
    # Redirect stderr to a temporary file rather than subprocess.PIPE so verbose
    # startup diagnostics cannot fill a 64 KiB pipe buffer and deadlock the child.
    stderr_file = tempfile.TemporaryFile()
    try:
        try:
            proc = subprocess.Popen(
                cmd,
                cwd=agent_dir,
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=stderr_file,
                env=env,
            )
        except OSError as exc:
            return None, f"failed to spawn {binary}: {exc}"
        try:
            time.sleep(settle_ms / 1000.0)
            if proc.poll() is not None:
                stderr_file.seek(0)
                stderr_tail = stderr_file.read().decode("utf-8", "replace")[-500:]
                return (
                    None,
                    f"pi --mode rpc exited prematurely during settle window (rc={proc.returncode}): {stderr_tail}",
                )
            rss_val = sample_pid_rss(proc.pid)
            if rss_val is None or rss_val <= 0:
                return None, f"failed to read VmRSS for pid {proc.pid}"
            return {"pid": proc.pid, "process_name": binary.name, "rss_bytes": rss_val}, None
        finally:
            if proc.stdin is not None:
                try:
                    proc.stdin.close()
                except OSError:
                    pass
            try:
                proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                proc.terminate()
                try:
                    proc.wait(timeout=2.0)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=2.0)
    finally:
        stderr_file.close()


def is_lowercase_nonzero_git_sha(raw: str) -> bool:
    return (
        len(raw) == 40
        and all(ch in "0123456789abcdef" for ch in raw)
        and not all(ch == "0" for ch in raw)
    )


def default_release_binary(workdir: Path) -> Path:
    env_target = os.environ.get("CARGO_TARGET_DIR", "").strip()
    if env_target:
        target_path = Path(env_target)
        if not target_path.is_absolute():
            target_path = workdir / target_path
        cand = target_path / "release/pi"
        if cand.exists():
            return cand
    return workdir / "target/release/pi"


def run_self_test() -> int:
    old_target = os.environ.pop("CARGO_TARGET_DIR", None)
    try:
        with tempfile.TemporaryDirectory(prefix="measure_idle_memory_selftest_") as tmp:
            root = Path(tmp)
            out_path = root / "idle_memory_rss.json"

            # 1. Missing binary fails closed.
            rc = run_cli(["--binary", str(root / "target/release/pi"), "--out", str(out_path), "--source-commit", "a" * 40])
            assert rc == 1
            assert not out_path.exists()

            # 2. Fake idle RPC binary stays alive until stdin EOF -> 5 unique PIDs, exact 22-field schema.
            fake_pi = root / "target/release/pi"
            fake_pi.parent.mkdir(parents=True, exist_ok=True)
            fake_pi.write_text(
                "#!/usr/bin/env python3\nimport sys\nsys.stdin.read()\n",
                encoding="utf-8",
            )
            fake_pi.chmod(fake_pi.stat().st_mode | stat.S_IEXEC)

            rc = run_cli(
                [
                    "--workdir",
                    str(root),
                    "--binary",
                    str(fake_pi),
                    "--settle-ms",
                    "120",
                    "--samples",
                    "5",
                    "--out",
                    str(out_path),
                    "--run-id",
                    "selftest-rss-1",
                    "--source-commit",
                    "a" * 40,
                    "--source-clean",
                ]
            )
            assert rc == 0, f"expected rc=0 on fake idle pi, got {rc}"
            payload = json.loads(out_path.read_text(encoding="utf-8"))
            assert tuple(sorted(payload.keys())) == tuple(sorted(REQUIRED_FIELDS))
            assert payload["schema"] == SCHEMA
            assert payload["sample_count"] == 5
            assert len({s["pid"] for s in payload["samples"]}) == 5
            max_rss = max(s["rss_bytes"] for s in payload["samples"])
            min_rss = min(s["rss_bytes"] for s in payload["samples"])
            assert payload["rss_bytes"] == max_rss
            assert payload["rss_spread_bytes"] == max_rss - min_rss
            assert any(s["pid"] == payload["pid"] and s["rss_bytes"] == payload["rss_bytes"] for s in payload["samples"])
            canonical_env = json.dumps(
                payload["bench_env"], sort_keys=True, separators=(",", ":"), ensure_ascii=False
            ).encode("utf-8")
            assert payload["bench_env_sha256"] == hashlib.sha256(canonical_env).hexdigest()

            # 3. Negative controls: invalid flags and premature exit fail closed and write nothing.
            neg_out = root / "neg_idle.json"
            assert run_cli(["--binary", str(fake_pi), "--samples", "4", "--out", str(neg_out), "--source-commit", "a" * 40]) == 1
            assert run_cli(["--binary", str(fake_pi), "--settle-ms", "50", "--out", str(neg_out), "--source-commit", "a" * 40]) == 1
            assert run_cli(["--binary", str(fake_pi), "--run-id", "a", "--correlation-id", "b", "--out", str(neg_out), "--source-commit", "a" * 40]) == 1
            assert run_cli(["--binary", str(fake_pi), "--out", str(neg_out), "--source-commit", "0" * 40]) == 1
            assert run_cli(["--binary", str(fake_pi), "--idle-state", "   ", "--out", str(neg_out), "--source-commit", "a" * 40]) == 1
            early_exit = root / "target/release/early_pi"
            early_exit.write_text("#!/usr/bin/env python3\nimport sys\nsys.exit(0)\n", encoding="utf-8")
            early_exit.chmod(early_exit.stat().st_mode | stat.S_IEXEC)
            assert run_cli(["--binary", str(early_exit), "--settle-ms", "150", "--out", str(neg_out), "--source-commit", "a" * 40]) == 2
            assert not neg_out.exists()
    finally:
        if old_target is not None:
            os.environ["CARGO_TARGET_DIR"] = old_target

    print("measure_idle_memory.py --self-test: PASS")
    return 0


def run_cli(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--workdir", type=Path, default=project_root())
    ap.add_argument("--binary", type=Path, default=None)
    ap.add_argument("--idle-state", default="startup_before_user_input")
    ap.add_argument("--allocator", default="system", choices=("system", "jemalloc"))
    ap.add_argument("--samples", type=int, default=DEFAULT_SAMPLE_COUNT)
    ap.add_argument("--settle-ms", type=int, default=DEFAULT_SETTLE_MS)
    ap.add_argument("--run-id", default=None)
    ap.add_argument("--correlation-id", default=None)
    ap.add_argument("--source-commit", default=None)
    ap.add_argument("--source-clean", action="store_true")
    ap.add_argument(
        "--out",
        type=Path,
        default=project_root() / "tests/perf/reports/release_evidence/idle_memory_rss.json",
    )
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)

    if args.self_test:
        return run_self_test()

    if args.run_id and args.correlation_id and args.run_id != args.correlation_id:
        print(
            f"FAIL: --run-id ({args.run_id!r}) and --correlation-id ({args.correlation_id!r}) must match",
            file=sys.stderr,
        )
        return 1
    idle_state = args.idle_state.strip()
    if not idle_state:
        print("FAIL: --idle-state must be non-empty", file=sys.stderr)
        return 1
    if idle_state != "startup_before_user_input":
        print(
            f"NOTE: --idle-state={idle_state!r} differs from canonical 'startup_before_user_input'",
            file=sys.stderr,
        )
    if args.samples < 5:
        print("FAIL: --samples must be >= 5 per src/perf_build.rs::verify_idle_rss_samples", file=sys.stderr)
        return 1
    if not (100 <= args.settle_ms <= 10_000):
        print("FAIL: --settle-ms must be in 100..=10000", file=sys.stderr)
        return 1

    binary = (args.binary or default_release_binary(args.workdir)).resolve()
    if not binary.is_file():
        print(f"FAIL: release binary not found: {binary} (no artifact written)", file=sys.stderr)
        return 1

    if args.source_commit is not None:
        source_commit = args.source_commit.strip().lower()
        source_dirty = not args.source_clean
    else:
        try:
            source_commit, source_dirty = git_source_identity(args.workdir)
            source_commit = source_commit.lower()
        except (OSError, subprocess.CalledProcessError) as exc:
            print(f"FAIL: could not determine git source identity in {args.workdir}: {exc}", file=sys.stderr)
            return 1
    if not is_lowercase_nonzero_git_sha(source_commit):
        print(
            f"FAIL: source_commit must be a 40-char lowercase non-zero git SHA, got {source_commit!r}",
            file=sys.stderr,
        )
        return 1
    if source_dirty:
        print(
            "NOTE: source_dirty=true (diagnostic measurement; strict release-budget gate requires clean HEAD)",
            file=sys.stderr,
        )
    if binary.parent.name != "release" or binary.name != "pi":
        print(
            f"NOTE: binary_path={binary} does not end with release/pi "
            "(strict verify_idle_rss_measurement_control requires a canonical release/pi path)",
            file=sys.stderr,
        )

    default_id = f"idle-rss-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}"
    run_id = args.run_id or args.correlation_id or default_id
    correlation_id = args.correlation_id or run_id

    samples: list[dict[str, Any]] = []
    seen_pids: set[int] = set()
    with tempfile.TemporaryDirectory(prefix="pi_idle_rss_agent_") as agent_tmp:
        agent_dir = Path(agent_tmp)
        for idx in range(args.samples):
            sample, err = measure_one_idle_process(
                binary, settle_ms=args.settle_ms, agent_dir=agent_dir
            )
            if err is not None or sample is None:
                print(f"FAIL (sample {idx + 1}/{args.samples}): {err}", file=sys.stderr)
                return 2
            if sample["pid"] in seen_pids:
                print(f"FAIL: duplicate PID {sample['pid']} observed", file=sys.stderr)
                return 2
            seen_pids.add(sample["pid"])
            samples.append(sample)

    max_sample = max(samples, key=lambda s: s["rss_bytes"])
    min_rss = min(s["rss_bytes"] for s in samples)
    rss_spread = max_sample["rss_bytes"] - min_rss
    bench_env, bench_env_sha256 = collect_bench_env(source_commit)

    artifact = {
        "schema": SCHEMA,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
        "run_id": run_id,
        "correlation_id": correlation_id,
        "source_commit": source_commit,
        "source_dirty": source_dirty,
        "pid": max_sample["pid"],
        "process_name": binary.name,
        "allocator": args.allocator,
        "binary_path": str(binary),
        "binary_sha256": sha256_file(binary),
        "rss_bytes": max_sample["rss_bytes"],
        "idle_state": idle_state,
        "cargo_profile": "release",
        "build_command": "cargo build --bin pi --release",
        "sample_count": len(samples),
        "samples": samples,
        "rss_spread_bytes": rss_spread,
        "settle_ms": args.settle_ms,
        "bench_env_source": "benches/bench_env.rs",
        "bench_env": bench_env,
        "bench_env_sha256": bench_env_sha256,
    }
    assert tuple(sorted(artifact.keys())) == tuple(sorted(REQUIRED_FIELDS))

    args.out.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = args.out.with_name(f".{args.out.name}.tmp.{os.getpid()}")
    try:
        tmp_path.write_text(json.dumps(artifact, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        tmp_path.replace(args.out)
    except Exception:
        tmp_path.unlink(missing_ok=True)
        raise

    print(
        f"wrote {args.out}: max_rss={max_sample['rss_bytes']} bytes "
        f"({max_sample['rss_bytes'] / (1024.0 * 1024.0):.2f} MiB) "
        f"across {len(samples)} unique PIDs (spread={rss_spread} bytes, "
        f"noise_score={bench_env['noise_score']})"
    )
    return 0


def main() -> int:
    return run_cli()


if __name__ == "__main__":
    sys.exit(main())
