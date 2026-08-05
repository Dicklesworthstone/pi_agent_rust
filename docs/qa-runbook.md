# QA Runbook and Failure Triage Playbook

Reference for running the test suite, interpreting failures, and reproducing issues.

**Bead:** bd-1f42.7.4
**Policy:** [docs/testing-policy.md](testing-policy.md)
**Rubric:** [docs/non-mock-rubric.json](non-mock-rubric.json)
**Coverage baseline:** [docs/coverage-baseline-map.json](coverage-baseline-map.json)

---

## Contract Checklist (DROPIN-171)

`docs/testing-policy.md` defines the normative contract `pi.parity.test_logging_contract.v1`.
Use this runbook section to validate that the contract remains intact.

### Required Cross-Suite Guarantees

| Guarantee | Validation Source |
|-----------|-------------------|
| Suite taxonomy is explicit (`unit`/`vcr`/`e2e`) | `tests/suite_classification.toml` |
| Test log schema remains `pi.test.log.v2` | `tests/common/logging.rs` validators |
| Artifact index schema remains `pi.test.artifact.v1` | `tests/common/logging.rs` validators |
| Schema evolution policy remains explicit and fail-closed | `docs/testing-policy.md` + `tests/common/logging.rs` v2-only validators |
| Evidence contract schema remains `pi.qa.evidence_contract.v1` | `docs/evidence-contract-schema.json` + schema tests |
| Failure digest taxonomy + replay metadata remain stable | `docs/evidence-contract-schema.json` |

### Contract Verification Commands

```bash
# Evidence contract schema + synthetic sample checks
cargo test --test validate_e2e_artifact_schema -- evidence_contract_schema --nocapture
cargo test --test validate_e2e_artifact_schema -- synthetic_evidence_contract --nocapture

# Log/artifact JSONL schema validation checks
cargo test --test e2e_artifact_retention_triage -- jsonl --nocapture

# Cross-run scenario/component filtering + stable-field comparison helpers
cargo test --test rpc_session_connector -- common::logging::tests::filter_log_records_by_scenario_and_component --nocapture
cargo test --test rpc_session_connector -- common::logging::tests::compare_log_streams_by_filter_ignores_trace_and_timing --nocapture

# Optional: full contract gate
cargo test --test validate_e2e_artifact_schema -- --nocapture
```

## Quick Start

### Fast smoke check (< 60 seconds)

```bash
./scripts/smoke.sh                  # lint + unit + VCR smoke targets
./scripts/smoke.sh --skip-lint      # skip fmt/clippy (faster inner loop)
./scripts/smoke.sh --only unit      # unit smoke only
./scripts/smoke.sh --json           # machine-readable summary to stdout
```

Artifacts: `tests/smoke_results/<timestamp>/smoke_summary.json`

### Full verification

```bash
./scripts/e2e/run_all.sh                        # full: lint + lib + all targets
./scripts/e2e/run_all.sh --profile ci            # CI profile: deterministic
./scripts/e2e/run_all.sh --profile quick         # fast: lint + lib + unit only

# Refresh certification dossier + remediation backlog artifacts
cargo test --test qa_certification_dossier -- certification_dossier --nocapture --exact
```

Artifacts: `tests/e2e_results/<timestamp>/summary.json`

### Suite-specific runs

```bash
# Unit tests only (no mocks, no fixtures, no VCR)
cargo test --all-targets --lib

# VCR/fixture replay tests
VCR_MODE=playback cargo test --all-targets

# Single test file
cargo test --test provider_contract

# Single test function
cargo test --test non_mock_rubric_gate -- rubric_has_required_top_level_keys
```

## Performance Workflow: Fast Loop vs Definitive Benchmarks

Use a two-speed workflow so agents can move quickly without treating partial data as a release claim.

| Mode | Use When | Command Pattern | Claim Strength |
|------|----------|-----------------|----------------|
| Fast inner loop | During active edits | File-scoped checks only (`cargo fmt --check -- <file>`, targeted `cargo test --test ...`) | **Non-authoritative**; developer feedback only |
| Definitive benchmark/certification pass | At integration/decision boundaries | Heavy runs offloaded with `rch exec -- ...` + required evidence artifacts regenerated | **Authoritative** for PERF-3X/release claims |

### Definitive Benchmark Gate (authoritative)

Before starting a long benchmark refresh, run the read-only preflight so missing
inputs are reported up front with the exact RCH commands and expected paths:

```bash
python3 scripts/perf/preflight_budget_inputs.py
```

`scripts/perf/orchestrate.sh` persists the same check as
`results/perf_budget_preflight_before_refresh.json` before the perf budget
suite can refresh `tests/perf/reports/budget_summary.json`. It also writes
`results/perf_artifact_staging_manifest.json` (`pi.perf.artifact_staging_manifest.v1`)
after artifact collection. Treat that staging manifest as blocked unless every
required contract has a fresh artifact with source path, retrieved local path,
mtime, checksum, and schema when the format provides one. If RCH exposes a
remote target prefix, set `PERF_REMOTE_TARGET_DIR` so the manifest records the
remote source path instead of the inferred local target path.

Perf runs also maintain an evidence cache at `PERF_EVIDENCE_CACHE_DIR`
(default: `$CARGO_TARGET_DIR/perf/evidence_cache`). Cached entries are reusable
only when their schema, command, git commit, build profile, run
ID/correlation ID, host/toolchain provenance, checksum, and TTL validate. The
preflight and staging JSON label reused artifacts with `source_kind=cache` or
`evidence_source=cache`; rejected cache entries stay visible in
`rejected_evidence_cache_entries`. Use `PI_PERF_EVIDENCE_CACHE_TTL_HOURS` to
shorten the maximum accepted cache lifetime.

When a cargo target directory lives outside the repository, do not ask an RCH
worker to infer budget inputs from that local path. Stage the required JSON,
JSONL, Criterion, and release-binary evidence under a repo-visible evidence root
and run the report with `PERF_EVIDENCE_DIR`:

```bash
PERF_EVIDENCE_DIR=tests/perf/reports \
  PI_GENERATE_PERF_BUDGET_REPORT=1 rch exec -- cargo test --test perf_budgets --profile perf generate_budget_report -- --nocapture
```

Both report-producing integration tests are read-only during ordinary test
runs. Refresh tracked artifacts only with their explicit opt-ins:

```bash
PI_GENERATE_PERF_BUDGET_REPORT=1 cargo test --test perf_budgets generate_budget_report -- --nocapture
PI_GENERATE_BENCH_SCHEMA_DOCS=1 cargo test --test bench_schema generate_schema_doc -- --nocapture
```

In `pi.perf.budget_summary.v2`, `performance_claims_authorized=true` governs
blanket quantitative performance copy. It therefore requires strict,
source-bound, same-run evidence for **all declared budgets**: every budget must
have data and PASS, including budgets that are not CI-enforced, and there must
be no data-contract failures. `budget_data_missing` and `budget_failed` are the
aggregate blocker codes; the CI-specific counts and blockers remain as
diagnostics. When that authoritative lineage is incomplete, the tracked report
is a canonical all-`NO_DATA` blocked sentinel and deliberately does not inspect
ambient Cargo targets, ignored artifacts, or filesystem mtimes. A complete
strict lineage is required before real artifact evaluation begins.

`tests/perf_budgets.rs` checks `PERF_EVIDENCE_DIR`/`PERF_EVIDENCE_DIRS` before
`CARGO_TARGET_DIR`, so the report can consume staged evidence that RCH actually
syncs instead of an unsynced local target directory.

The orchestrator's `env_fingerprint.json` is cgroup-aware. It records cgroup v2
CPU quota, cpuset size, memory limit, NUMA node count, host-vs-container
caveats, and a `budget_profile` whose target CPU/memory values use constrained
container resources when they are lower than the host totals.

Run heavyweight checks with remote offload:

```bash
rch exec -- cargo test --test bench_scenario_runner -- --nocapture
rch exec -- cargo test --test perf_budgets -- --nocapture
rch exec -- cargo test --test release_evidence_gate -- --nocapture
rch exec -- cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact
```

Treat benchmark outcomes as definitive only when all required artifacts are present and schema-valid:

- `tests/perf/reports/phase1_matrix_validation.json` (`pi.perf.phase1_matrix_validation.v1`)
- `tests/full_suite_gate/full_suite_verdict.json`
- `tests/full_suite_gate/certification_verdict.json`
- `tests/full_suite_gate/extension_remediation_backlog.json` (`pi.qa.extension_remediation_backlog.v1`)

---

## Test Suite Classification

Every test file belongs to exactly one suite. See `tests/suite_classification.toml` for the canonical mapping.

| Suite | What it tests | Execution command |
|-------|---------------|-------------------|
| **unit** | Pure logic, parsing, serialization, state machines. No mocks, fixtures, or VCR. | `cargo test --lib` + curated `--test` targets |
| **vcr** | Provider streaming, HTTP client, conformance against recorded data. | `VCR_MODE=playback cargo test` |
| **e2e** | Full system with real providers, network, or tmux. | `PI_E2E=1 cargo test --test e2e_*` |

---

## Artifact Locations

| Artifact | Location | Content |
|----------|----------|---------|
| Smoke summary | `tests/smoke_results/<ts>/smoke_summary.json` | Pass/fail per target, duration |
| Smoke event log | `tests/smoke_results/<ts>/smoke_log.jsonl` | Per-event structured log |
| E2E summary | `tests/e2e_results/<ts>/summary.json` | Full run summary |
| E2E evidence | `tests/e2e_results/<ts>/evidence_contract.json` | Evidence contract |
| E2E replay bundle | `tests/e2e_results/<ts>/replay_bundle.json` | Consolidated replay commands + env context |
| E2E failure diagnostics | `tests/e2e_results/<ts>/failure_diagnostics_index.json` | Per-suite digest index |
| Per-suite failure digest | `tests/e2e_results/<ts>/<suite>/failure_digest.json` | Root cause + replay commands |
| Per-suite failure timeline | `tests/e2e_results/<ts>/<suite>/failure_timeline.jsonl` | Ordered failure events |
| E2E triage diff | `tests/e2e_results/<ts>/triage_diff.json` | Baseline vs current comparison |
| E2E scenario matrix | `docs/e2e_scenario_matrix.json` | Canonical workflow-to-suite coverage map |
| Conformance report | `tests/ext_conformance/reports/conformance_summary.json` | Extension conformance |
| CI gate verdict | `tests/full_suite_gate/full_suite_verdict.json` | Full-suite gate result |
| CI preflight verdict | `tests/full_suite_gate/preflight_verdict.json` | Preflight fast-fail result |
| CI certification verdict | `tests/full_suite_gate/certification_verdict.json` | Full certification result |
| Extension remediation backlog | `tests/full_suite_gate/extension_remediation_backlog.json` | Non-pass extension remediation queue (`pi.qa.extension_remediation_backlog.v1`) |
| CI waiver audit | `tests/full_suite_gate/waiver_audit.json` | Waiver lifecycle audit |
| CI replay bundle | `tests/full_suite_gate/replay_bundle.json` | Gate failure replay commands |
| Compliance report | `target/compliance-report.json` | Module compliance (set `COMPLIANCE_REPORT=1`) |
| Coverage baseline | `docs/coverage-baseline-map.json` | Line/function coverage per module |
| VCR cassettes | `tests/fixtures/vcr/` | Recorded HTTP interactions |
| Test failure log | `target/test-failures.jsonl` | Structured failure diagnostics |

---

## Failure Triage Playbook

### Step 1: Identify the failure class

| Signature | Likely Class | Next Action |
|-----------|-------------|-------------|
| `assertion failed` in `provider_*` test | Provider regression | Check VCR cassette freshness; verify `api_key` is set in `StreamOptions` |
| `missing Start event` | Streaming auth failure | Ensure `api_key: Some("vcr-playback".to_string())` in `StreamOptions` for VCR tests |
| `request URL mismatch` in VCR | Model ID drift | VCR uses strict URL matching. Ensure model ID in test matches cassette URL path |
| `connection refused` | Missing test infrastructure | Check if mock server or VCR is configured; verify `VCR_MODE` env var |
| `DummyProvider` / `NullSession` in unit test | Policy violation | Move test to VCR suite or replace double with real implementation |
| `SIGSEGV` in `llvm-cov` | LLVM bug with branch coverage on large files | Use per-file `llvm-cov export -sources FILE -summary-only` workaround. 63/107 files work; 44 files SIGSEGV. See `docs/coverage-baseline-map.json` for the full file list and deterministic command path (bd-1f42.1.5) |
| `thread panicked` in extension test | Extension dispatcher issue | Check `src/extension_dispatcher.rs`; review mock stub usage |
| Flaky: passes locally, fails on CI | Non-determinism | Classify per flake taxonomy (FLAKE-TIMING/ENV/NET/RES/EXT/LOGIC) |
| `No such file or directory` for cassette | Missing VCR fixture | Record new cassette or check cassette naming convention |
| `too many open files` | Resource exhaustion | Increase `ulimit -n`; check for leaked file descriptors |

### Step 2: Reproduce locally

```bash
# Run the specific failing test with output
cargo test --test <test_file> -- <test_name> --nocapture

# With VCR playback forced
VCR_MODE=playback cargo test --test <test_file> -- <test_name> --nocapture

# With debug logging
RUST_LOG=debug cargo test --test <test_file> -- <test_name> --nocapture

# With backtrace
RUST_BACKTRACE=1 cargo test --test <test_file> -- <test_name> --nocapture
```

### Step 3: Check VCR cassette integrity

If the failure involves provider/streaming tests:

```bash
# List cassettes for a provider
ls tests/fixtures/vcr/verify_<provider>_*.json

# Verify cassette is valid JSON
python3 -m json.tool tests/fixtures/vcr/<cassette>.json > /dev/null

# Check request URL in cassette matches test expectations
python3 -c "
import json
with open('tests/fixtures/vcr/<cassette>.json') as f:
    d = json.load(f)
    for i in d['interactions']:
        print(i['request']['url'])
"
```

### Step 4: Review compliance status

```bash
# Generate compliance report
COMPLIANCE_REPORT=1 cargo test --test non_mock_compliance_gate

# Check rubric integrity
cargo test --test non_mock_rubric_gate

# View module coverage status
python3 -c "
import json
with open('docs/coverage-baseline-map.json') as f:
    d = json.load(f)
    for cp in d['critical_paths']:
        print(f\"{cp['area']}: {cp['coverage']['line_pct']:.1f}% line, {cp['coverage']['function_pct']:.1f}% function, {cp['coverage']['branch_pct']:.1f}% branch(fallback)\")
"
```

---

## Replay Workflow

The E2E harness supports deterministic replay for failure reproduction.

### One-command replay (from summary.json)

```bash
# Rerun only failed suites from a previous run
./scripts/e2e/run_all.sh --rerun-from tests/e2e_results/<ts>/summary.json

# Compare current run against a baseline
./scripts/e2e/run_all.sh --diff-from tests/e2e_results/<baseline>/summary.json
```

The `--rerun-from` flag reads `failed_names` from the summary, re-runs only those suites, and
auto-sets `--diff-from` to the source summary for triage diff generation.

### Replay bundle artifact

After a run completes, the harness emits `replay_bundle.json` (schema `pi.e2e.replay_bundle.v1`)
alongside `summary.json`. The bundle consolidates:

- **`one_command_replay`**: Single command to reproduce all failures.
- **`environment`**: Profile, shard context, VCR mode, rustc version, git SHA, OS.
- **`failed_suites`**: Per-suite entries with runner, cargo, and targeted replay commands plus failure digest paths.
- **`failed_unit_targets`**: Per-target cargo replay commands.

```bash
# View the one-command replay from a previous run
python3 -c "
import json
with open('tests/e2e_results/<ts>/replay_bundle.json') as f:
    b = json.load(f)
    print(b['one_command_replay'])
    for s in b['failed_suites']:
        print(f\"  {s['suite']}: {s['cargo_replay']}\")
"
```

### Per-suite failure digest

Each failed suite also gets a `failure_digest.json` (schema `pi.e2e.failure_digest.v1`) with:

- `remediation_pointer.replay_command` (runner-level)
- `remediation_pointer.suite_replay_command` (cargo test)
- `remediation_pointer.targeted_test_replay_command` (single test)
- Root cause classification and first failing assertion

### Triage diff

When a baseline is provided (via `--diff-from` or auto-set by `--rerun-from`), the harness
generates `triage_diff.json` (schema `pi.e2e.triage_diff.v1`) containing:

- Regressions, new failures, fixed tests, and unresolved failures.
- `recommended_commands.runner_repro_command`: One command to re-run all problem suites.
- `recommended_commands.ranked_repro_commands`: Prioritized list of per-target commands.
- `ranked_diagnostics`: Severity-ranked list with recommended replay commands.

### Certification backlog refresh

When certification artifacts are refreshed, regenerate the extension remediation backlog in the same
run so diagnostics and release gates consume the same evidence set:

```bash
cargo test --test qa_certification_dossier -- certification_dossier --nocapture --exact
```

Produced artifacts:

- `tests/full_suite_gate/certification_dossier.json`
- `tests/full_suite_gate/certification_dossier.md`
- `tests/full_suite_gate/extension_remediation_backlog.json`
- `tests/full_suite_gate/extension_remediation_backlog.md`

---

## CI Gate Lanes

The full-suite CI gate operates in two lanes (bd-1f42.8.8.1):

### Preflight fast-fail lane

Evaluates **blocking gates only** and stops at the first failure. Used for fast feedback
in PR checks.

```bash
cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact
```

Artifact: `tests/full_suite_gate/preflight_verdict.json` (schema `pi.ci.preflight_lane.v1`)

### Full certification lane

Evaluates **all gates** (blocking + non-blocking), generates a waiver audit, and produces
a comprehensive verdict with promotion rules and rerun guidance.

```bash
cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact
```

Artifacts:
- `tests/full_suite_gate/certification_verdict.json`
- `tests/full_suite_gate/certification_events.jsonl`
- `tests/full_suite_gate/certification_report.md`

### Final >=3x Go/No-Go Decision Workflow (bd-3ar8v.6.5)

Use this workflow only at definitive release-decision boundaries. Treat missing
or stale evidence as `NO-GO` (fail-closed), never as a warning.

1. Regenerate authoritative certification evidence (offloaded):

```bash
rch exec -- cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact
rch exec -- cargo test --test release_evidence_gate -- --nocapture
rch exec -- cargo test --test qa_certification_dossier -- certification_dossier --nocapture --exact
```

2. Confirm required artifacts exist and are current:
- `tests/full_suite_gate/full_suite_verdict.json`
- `tests/full_suite_gate/certification_verdict.json`
- `tests/full_suite_gate/practical_finish_checkpoint.json`
- `tests/full_suite_gate/extension_remediation_backlog.json`
- `tests/perf/reports/opportunity_matrix.json`
- `tests/perf/reports/parameter_sweeps.json`

3. Enforce final gate pass criteria from `full_suite_verdict.json`:
- `perf3x_bead_coverage = pass`
- `practical_finish_checkpoint = pass`
- `extension_remediation_backlog = pass`
- `opportunity_matrix_integrity = pass`
- `parameter_sweeps_integrity = pass`

4. Verify practical-finish output is docs/report-only residual scope:
- `status = "pass"`
- `technical_open_count = 0`
- `residual_open_scope` is `none` or `docs_or_report_only`

5. Decide:
- `GO`: all criteria above pass with fresh artifacts.
- `NO-GO`: any missing artifact, stale/invalid schema, or failed gate.

Quick gate-status extractor:

```bash
python3 - <<'PY'
import json
from pathlib import Path

required = {
    "perf3x_bead_coverage",
    "practical_finish_checkpoint",
    "extension_remediation_backlog",
    "opportunity_matrix_integrity",
    "parameter_sweeps_integrity",
}
path = Path("tests/full_suite_gate/full_suite_verdict.json")
if not path.exists():
    raise SystemExit("NO-GO: missing tests/full_suite_gate/full_suite_verdict.json")
data = json.loads(path.read_text(encoding="utf-8"))
statuses = {g["id"]: g.get("status") for g in data.get("gates", []) if g.get("id") in required}
missing = sorted(required - set(statuses))
failed = sorted(g for g, s in statuses.items() if s != "pass")
if missing or failed:
    raise SystemExit(f"NO-GO: missing={missing} failed={failed}")
print("GO candidate: all required PERF-3X final gates are pass")
PY
```

### Drop-in certification contract gate (bd-35t7i)

Before release messaging can claim strict drop-in parity, evaluate
`docs/contracts/dropin-certification-contract.json` and emit
`docs/evidence/dropin-certification-verdict.json` (`pi.dropin.certification_verdict.v1`).

Blocking rule:
- if `overall_verdict != CERTIFIED`, release language must not claim strict drop-in replacement.

Parity incident handling for this gate is defined in
`docs/ci-operator-runbook.md` under
**Parity Incident Response (DROPIN-162)**. Treat `overall_verdict != CERTIFIED`
or a missing verdict artifact on release paths as a parity incident, not a docs-only warning.

### Gate reproduce commands

Every gate includes a `reproduce_command` field. To replay a specific gate failure:

```bash
# View all gate reproduce commands
python3 -c "
import json
with open('tests/full_suite_gate/full_suite_verdict.json') as f:
    v = json.load(f)
    for g in v['gates']:
        if g.get('status') == 'fail':
            print(f\"{g['id']}: {g.get('reproduce_command', 'N/A')}\")
"
```

---

## PERF-3X Regression Triage (bd-3ar8v.6.4)

Use this flow for permanent performance-gate incidents. Missing or stale evidence
is a failure condition, not a warning.

### Detection (fail-closed artifact checks)

After running the full certification lane, the following artifacts must exist and
match expected schemas:

- `tests/full_suite_gate/full_suite_verdict.json` (`pi.ci.full_suite_gate.v1`)
- `tests/full_suite_gate/certification_verdict.json` (`pi.ci.certification_lane.v1`)
- `tests/full_suite_gate/perf3x_bead_coverage_audit.json` (`pi.perf3x.bead_coverage.audit.v1`)
- `tests/full_suite_gate/practical_finish_checkpoint.json` (`pi.perf3x.practical_finish_checkpoint.v1`)
- `tests/perf/reports/budget_summary.json` (`pi.perf.budget_summary.v2`)
- `tests/perf/reports/perf_comparison.json` (`pi.ext.perf_comparison.v1`)
- `tests/perf/reports/stress_triage.json` (`pi.ext.stress_triage.v1`)
- `tests/perf/reports/parameter_sweeps.json` (`pi.perf.parameter_sweeps.v1`)

```bash
python3 - <<'PY'
import json
import sys
from pathlib import Path

required = {
    "tests/full_suite_gate/full_suite_verdict.json": "pi.ci.full_suite_gate.v1",
    "tests/full_suite_gate/certification_verdict.json": "pi.ci.certification_lane.v1",
    "tests/full_suite_gate/perf3x_bead_coverage_audit.json": "pi.perf3x.bead_coverage.audit.v1",
    "tests/full_suite_gate/practical_finish_checkpoint.json": "pi.perf3x.practical_finish_checkpoint.v1",
    "tests/perf/reports/budget_summary.json": "pi.perf.budget_summary.v2",
    "tests/perf/reports/perf_comparison.json": "pi.ext.perf_comparison.v1",
    "tests/perf/reports/stress_triage.json": "pi.ext.stress_triage.v1",
    "tests/perf/reports/parameter_sweeps.json": "pi.perf.parameter_sweeps.v1",
}

issues = []
for rel, expected_schema in required.items():
    path = Path(rel)
    if not path.exists():
        issues.append(f"missing: {rel}")
        continue
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        issues.append(f"invalid-json: {rel} ({exc})")
        continue
    schema = payload.get("schema")
    generated_at = payload.get("generated_at")
    if schema != expected_schema:
        issues.append(
            f"schema-mismatch: {rel} expected={expected_schema} actual={schema}"
        )
    if not isinstance(generated_at, str) or not generated_at.strip():
        issues.append(f"missing-generated_at: {rel}")

if issues:
    print("FAIL (fail-closed):")
    for issue in issues:
        print(f" - {issue}")
    sys.exit(1)

print("PASS: PERF-3X gate artifacts are present with expected schema+timestamp fields.")
PY
```

### PERF-3X signature quick-reference

Use this quick table for first-response triage before deeper log analysis.

| Gate | Failure signature | Primary artifacts | Replay command | First remediation action |
|------|-------------------|-------------------|----------------|--------------------------|
| `parameter_sweeps_integrity` | `full_suite_verdict.json` gate status = `fail` with readiness/source contract drift details | `tests/perf/reports/parameter_sweeps.json`, `tests/perf/reports/parameter_sweeps_events.jsonl`, `tests/perf/reports/phase1_matrix_validation.json` | `rch exec -- cargo test --test release_evidence_gate -- parameter_sweeps_contract_links_phase1_matrix_and_readiness --nocapture --exact` | Rebuild parameter sweep artifact from latest phase1 matrix and restore readiness/source_identity coherence. |
| `practical_finish_checkpoint` | Gate detail reports technical PERF-3X issues still open or fail-closed checkpoint read error | `tests/full_suite_gate/practical_finish_checkpoint.json`, `.beads/issues.jsonl` | `rch exec -- cargo test --test ci_full_suite_gate -- practical_finish_report_fails_when_technical_open_issues_remain --nocapture --exact` | Close/re-scope technical PERF-3X issues so only docs/report residual scope remains open. |

### Attribution (log-query playbooks)

Unit diagnostics (`test-log.jsonl` for a unit target):

```bash
python3 - <<'PY'
import json
from pathlib import Path

path = Path("tests/e2e_results/<ts>/unit/<target>/test-log.jsonl")
if not path.exists():
    raise SystemExit(f"missing unit log: {path}")

for line in path.read_text(encoding="utf-8").splitlines():
    if not line.strip():
        continue
    rec = json.loads(line)
    if rec.get("status") in {"fail", "error"}:
        print(
            rec.get("ts"),
            rec.get("component", "<component>"),
            rec.get("scenario_id", "<scenario>"),
            rec.get("message", "<no-message>"),
        )
PY
```

E2E diagnostics (`test-log.jsonl` for a suite):

```bash
python3 - <<'PY'
import json
from pathlib import Path

path = Path("tests/e2e_results/<ts>/<suite>/test-log.jsonl")
if not path.exists():
    raise SystemExit(f"missing e2e log: {path}")

for line in path.read_text(encoding="utf-8").splitlines():
    if not line.strip():
        continue
    rec = json.loads(line)
    if rec.get("status") in {"fail", "error"} or rec.get("level") in {"warn", "error"}:
        print(
            rec.get("ts"),
            rec.get("level", "<level>"),
            rec.get("scenario_id", "<scenario>"),
            rec.get("message", "<no-message>"),
        )
PY
```

Perf diagnostics (budget/comparison/stress/parameter-sweeps event streams):

```bash
python3 - <<'PY'
import json
from pathlib import Path

event_paths = [
    Path("tests/perf/reports/budget_events.jsonl"),
    Path("tests/perf/reports/perf_comparison_events.jsonl"),
    Path("tests/perf/reports/stress_events.jsonl"),
    Path("tests/perf/reports/parameter_sweeps_events.jsonl"),
]

for path in event_paths:
    if not path.exists():
        print(f"[MISSING] {path}")
        continue
    print(f"\n== {path} ==")
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        rec = json.loads(line)
        status = str(rec.get("status", "")).lower()
        if status in {"fail", "error", "warn"}:
            print(
                rec.get("ts"),
                status,
                rec.get("scenario_id", rec.get("name", "<name>")),
                rec.get("message", "<no-message>"),
            )
PY
```

### User-facing diagnostics workflow (durability/resume/extension/build-profile)

Use this workflow for user-facing "it is slow" incidents. Keep the scenario, replay
command, and artifact pointers together so triage can be reproduced exactly.

#### Swarm coordination troubleshooting

- Preflight command:
  `pi doctor --only swarm --format json`
- Confirms:
  Beads JSONL health, stale `in_progress` work, Agent Mail status/inbox/reservations,
  git dirty-state, `rch status`, and `CARGO_TARGET_DIR`/`TMPDIR` headroom.
- Admission interpretation:
  In JSON output, inspect the swarm finding whose `data.schema` is
  `pi.doctor.swarm_admission.v1`. Its `action` is `admit`, `backpressure`,
  or `deny`; `pressure_dimension` names the active bottleneck; `planned_budgets`
  lists the current agent/tool/extension/RCH limits; `live_counts` records the
  counts used for the decision; `stale_data_warnings` must be empty before
  treating an `admit` as healthy; and `next_actions` is the operator handoff.
- Next actions:
  Contact active owners through Agent Mail before resetting stale beads, resolve
  reservation conflicts before editing overlapping files, and move heavyweight
  Cargo checks to `/data/tmp/pi_agent_rust_cargo/<agent>/` when headroom is low.

#### Durability troubleshooting

- Replay command:
  `rch exec -- cargo test --test release_evidence_gate -- parameter_sweeps_contract_links_phase1_matrix_and_readiness --nocapture --exact`
- Artifact pointers:
  `tests/perf/reports/parameter_sweeps.json`,
  `tests/perf/reports/parameter_sweeps_events.jsonl`,
  `tests/perf/reports/budget_summary.json`

#### Resume troubleshooting

- Replay command:
  `./scripts/e2e/run_all.sh --rerun-from <scenario-id> --diff-from <baseline-dir>`
- Artifact pointers:
  `tests/e2e_results/<ts>/<suite>/summary.json`,
  `tests/e2e_results/<ts>/<suite>/test-log.jsonl`

#### Extension troubleshooting

- Replay command:
  `rch exec -- cargo test --test ci_full_suite_gate -- extension_remediation_backlog_sub_gate_is_blocking_and_points_to_dedicated_artifact --nocapture --exact`
- Artifact pointers:
  `tests/full_suite_gate/extension_remediation_backlog.json`,
  `tests/full_suite_gate/certification_dossier.json`,
  `tests/ext_conformance/reports/conformance_summary.json`

#### Build-profile troubleshooting

- Replay commands:
  `rch exec -- cargo test --test bench_scenario_runner -- --nocapture`
  `rch exec -- cargo test --test perf_budgets -- --nocapture`
- Artifact pointers:
  `tests/perf/reports/perf_comparison.json`,
  `tests/perf/reports/perf_comparison_events.jsonl`,
  `tests/perf/reports/stress_triage.json`

---

## Waiver Lifecycle

CI gates can be temporarily bypassed with auditable waivers (bd-1f42.8.8.1).

### Adding a waiver

Add a `[waiver.<gate_id>]` entry to `tests/suite_classification.toml`:

```toml
[waiver.ext_must_pass]
owner = "AgentName"
created = "2026-02-13"
expires = "2026-02-27"            # Max 30 days from created
bead = "bd-XXXX"
reason = "Blocked by upstream QuickJS bug"
scope = "both"                    # "full", "preflight", or "both"
remove_when = "QuickJS fix merged and all blocking extension conformance gates pass"
```

### Waiver rules

- Maximum duration: 30 days (must renew or fix before expiry).
- Expired waivers cause CI hard failure via the `waiver_lifecycle` gate.
- Waivers expiring within 3 days trigger warnings.
- The `gate_id` must match a gate in `ci_full_suite_gate.rs`.
- All 7 fields are required (owner, created, expires, bead, reason, scope, remove_when).

### Auditing waivers

```bash
# Run the standalone waiver audit
cargo test --test ci_full_suite_gate -- waiver_lifecycle_audit --nocapture --exact
```

Artifact: `tests/full_suite_gate/waiver_audit.json` (schema `pi.ci.waiver_audit.v1`)

---

## Extension Failure Dossier Interpretation

When an extension fails conformance testing:

1. **Check conformance summary:**
   ```bash
   python3 -c "
   import json
   with open('tests/ext_conformance/reports/conformance_summary.json') as f:
       d = json.load(f)
       print(f\"Pass: {d.get('pass_count', '?')}, Fail: {d.get('fail_count', '?')}, N/A: {d.get('na_count', '?')}\")
   "
   ```

2. **Review the failure detail:** Failure dossiers (when available from bd-1f42.4.8) contain:
   - Extension ID and version
   - Input fixture used
   - Expected vs actual output
   - Provider compatibility notes
   - One-command reproduction

3. **Common extension failure patterns:**

   | Pattern | Cause | Fix |
   |---------|-------|-----|
   | Schema validation error | Extension output doesn't match expected shape | Update fixture or fix extension output normalization |
   | Timeout | Extension takes too long to execute | Check for blocking I/O or missing async boundaries |
   | Policy denial | Extension lacks required capability | Review capability policy in `src/extension_dispatcher.rs` |
   | Missing host call | Extension uses unsupported host call op | Check protocol method dispatch coverage |

---

## Smoke Suite Usage

The smoke suite (`scripts/smoke.sh`) is designed for pre-push validation:

**Targets covered:**

| Suite | Targets | What it catches |
|-------|---------|-----------------|
| Unit | `model_serialization`, `config_precedence`, `session_conformance`, `error_types`, `compaction`, `security_budgets` | Core data model regressions |
| VCR | `provider_streaming`, `error_handling`, `http_client`, `sse_strict_compliance`, `model_registry`, `provider_factory` | Provider/HTTP/SSE regressions |

**When to run:**
- Before every `git push`
- After modifying `src/model.rs`, `src/providers/`, `src/sse.rs`, `src/config.rs`
- After updating VCR cassettes

**Interpreting smoke output:**
- Exit code 0: all pass
- Exit code 1: at least one failure (check `smoke_summary.json`)
- `--json` flag outputs machine-readable summary for scripting

---

## CI Gate Thresholds

The CI promotion gate (`.github/workflows/ci.yml`) evaluates:

| Metric | Default | Override Variable |
|--------|---------|-------------------|
| Promotion mode | `strict` | `CI_GATE_PROMOTION_MODE` |
| Min pass rate | 80.0% | `CI_GATE_MIN_PASS_RATE_PCT` |
| Max failures | 36 | `CI_GATE_MAX_FAIL_COUNT` |
| Max N/A | 170 | `CI_GATE_MAX_NA_COUNT` |

Emergency rollback: Set `CI_GATE_PROMOTION_MODE=rollback` to emit warnings without blocking.

---

## Per-Module Coverage Thresholds

From `docs/non-mock-rubric.json`:

| Module | Criticality | Line Floor | Function Floor | Line Target | Function Target |
|--------|-------------|------------|----------------|-------------|-----------------|
| agent_loop | critical | 75% | 70% | 85% | 80% |
| tools | critical | 75% | 72% | 85% | 82% |
| providers | critical | 82% | 79% | 90% | 88% |
| extensions | critical | 80% | 69% | 88% | 80% |
| session | high | 76% | 74% | 85% | 82% |
| auth | high | 72% | 70% | 82% | 78% |
| error | high | 72% | 70% | 82% | 78% |
| model | high | 74% | 72% | 84% | 80% |
| sse | high | 76% | 74% | 86% | 82% |
| config | medium | 70% | 68% | 80% | 76% |
| compaction | medium | 70% | 68% | 80% | 76% |
| vcr | medium | 68% | 66% | 78% | 74% |
| rpc | medium | 70% | 68% | 80% | 76% |
| interactive | low | 60% | 58% | 72% | 68% |

**Floor** = CI fails below this. **Target** = aspirational goal.

---

## Quarantine Workflow

Flaky tests are quarantined in `tests/suite_classification.toml`:

1. **Detect**: Test fails on CI but passes on retry (same commit)
2. **Classify**: Assign a `FLAKE-*` category (TIMING/ENV/NET/RES/EXT/LOGIC)
3. **Quarantine**: Add entry to `[quarantine.<test_stem>]` with all 9 required fields
4. **Fix**: Land fix within the category's window (7 or 14 days)
5. **Restore**: Remove quarantine entry after 3 clean CI runs

Maximum quarantine window: 14 days. See `docs/testing-policy.md` for full escalation ladder.
