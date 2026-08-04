# CI Operator Runbook: Failure Signatures to Replay Commands

Maps common CI failure signatures to exact replay commands, key artifact paths,
and remediation steps.

**Bead:** bd-1f42.8.9
**Policy:** [docs/testing-policy.md](testing-policy.md)
**QA Runbook:** [docs/qa-runbook.md](qa-runbook.md)

---

## Quick Reference: Replay from Any Failure

```bash
# 1. Replay all failed suites from a previous E2E run
./scripts/e2e/run_all.sh --rerun-from tests/e2e_results/<ts>/summary.json

# 2. Replay a single suite
cargo test --test <suite_name> -- --nocapture

# 3. Replay a single test function
cargo test --test <suite_name> <test_name> -- --nocapture

# 4. Replay with debug output
RUST_LOG=debug RUST_BACKTRACE=1 cargo test --test <suite_name> -- --nocapture

# 5. Replay CI gate failures
cargo test --test ci_full_suite_gate -- full_suite_gate --nocapture --exact
```

---

## Failure Signature Map

### Completion audit closeout gate failure

**Signature:** `Completion audit closeout gate self-test` fails or
`scripts/check_completion_audit_gate.py` exits non-zero for a closeout artifact.

**Artifacts:**
- Completion audit JSON from `scripts/build_completion_audit.py`
- `tests/fixtures/completion_audit_gate/scenarios.json`
- `tests/fixtures/completion_audit_gate/goldens/*.json`

**Replay:**
```bash
python3 scripts/check_completion_audit_gate.py --self-test \
  --generated-at 2026-01-02T03:04:05+00:00

python3 scripts/check_completion_audit_gate.py \
  --audit-json docs/evidence/<completion-audit>.json
```

**Remediation:**
1. Read `blockers[*].kind` and `operator_next_actions` in the gate JSON.
2. For `missing_push`, push the closeout commit and regenerate the audit.
3. For `failed_command`, rerun the exact command after fixing the failure and include the passing transcript.
4. For `proxy_only_evidence`, replace narrative or indirect proof with direct command, artifact, git, or Beads evidence.
5. For `missing_artifact`, create or correct the artifact path and rerun the audit.
6. For `unresolved_gap`, close the gap or create an active owner bead before closeout.

The gate is intentionally lightweight: it reads existing JSON only. It must not
run Cargo, call live providers, mutate Beads, send Agent Mail, launch RCH, or
delete files. Any future heavy validation referenced by the audit must use the
repo's RCH-backed command form.

### Non-mock compliance gate failure

**Signature:** `non_mock_compliance_gate ... FAILED`

**Artifacts:**
- `docs/non-mock-rubric.json` (rubric thresholds)
- `docs/test_double_inventory.json` (current inventory)

**Replay:**
```bash
cargo test --test non_mock_compliance_gate -- --nocapture
```

**Remediation:**
1. Check which module fell below its floor threshold.
2. Review `docs/non-mock-rubric.json` for the affected module's floor values.
3. Migrate mock/stub usages to VCR or real implementations.
4. See `docs/testing-policy.md` "Allowlisted Exceptions" for the approval process.

---

### Extension conformance gate failure

**Signature:** `conformance_must_pass_gate ... FAILED`

**Artifacts:**
- `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json`
- `tests/ext_conformance/reports/conformance_summary.json`

**Replay:**
```bash
cargo test --test ext_conformance_generated --features ext-conformance \
  -- conformance_must_pass_gate --nocapture --exact
```

**Remediation:**
1. Check `conformance_summary.json` for pass/fail/N/A counts.
2. Look for newly failing extensions in the summary.
3. Common causes: missing node shim, new hostcall not dispatched, QuickJS module resolution.
4. See `docs/conformance-operator-playbook.md` for debugging workflows.

---

### Cross-platform matrix failure

**Signature:** `cross_platform_matrix ... FAILED`

**Artifacts:**
- `tests/cross_platform_reports/linux/platform_report.json`

**Replay:**
```bash
cargo test --test ci_cross_platform_matrix -- cross_platform_matrix --nocapture --exact
```

**Remediation:**
1. Read the platform report to identify which checks failed.
2. Common causes: missing system dependencies, path separator issues, permission differences.
3. Fix the platform-specific code and re-run.

---

### Evidence bundle validation failure

**Signature:** `build_evidence_bundle ... FAILED`

**Artifacts:**
- `tests/evidence_bundle/index.json`

**Replay:**
```bash
cargo test --test ci_evidence_bundle -- build_evidence_bundle --nocapture --exact
```

**Remediation:**
1. Evidence bundle validates that all required artifacts exist and are well-formed.
2. Check for missing artifact files (summary.json, environment.json, etc.).
3. Ensure `scripts/e2e/run_all.sh` completed all post-run phases.

---

### Certification remediation backlog missing or stale

**Signature:** certification/readiness checks fail on missing
`extension_remediation_backlog.json` or schema mismatch.

**Artifacts:**
- `tests/full_suite_gate/certification_dossier.json`
- `tests/full_suite_gate/extension_remediation_backlog.json`
- `tests/full_suite_gate/extension_remediation_backlog.md`

**Replay:**
```bash
cargo test --test qa_certification_dossier -- certification_dossier --nocapture --exact
```

**Remediation:**
1. Regenerate certification artifacts and backlog in a single run (command above).
2. Verify backlog schema is `pi.qa.extension_remediation_backlog.v1`.
3. Ensure the backlog summary/entries are non-empty when conformance failures exist.
4. Re-run dependent gates after artifact refresh.

---

### Suite classification guard failure

**Signature:** `suite_classification` gate fails

**Artifacts:**
- `tests/suite_classification.toml`

**Replay:**
```bash
cargo test --test ci_full_suite_gate -- full_suite_gate --nocapture --exact
```

**Remediation:**
1. A new test file in `tests/` is not listed in `tests/suite_classification.toml`.
2. Classify the file into `[suite.unit]`, `[suite.vcr]`, or `[suite.e2e]`.
3. Keep entries sorted alphabetically within each suite.

---

### Waiver lifecycle failure

**Signature:** `waiver_lifecycle_audit ... FAILED` or `waiver_lifecycle` gate fails

**Artifacts:**
- `tests/full_suite_gate/waiver_audit.json`
- `tests/suite_classification.toml` (waiver entries)

**Replay:**
```bash
cargo test --test ci_full_suite_gate -- waiver_lifecycle_audit --nocapture --exact
```

**Remediation:**
1. Check `waiver_audit.json` for expired or invalid waivers.
2. Expired waivers must be either renewed (new `expires` date, max +30 days) or removed.
3. Invalid waivers are missing required fields; add all 7 fields.
4. See `docs/qa-runbook.md` "Waiver Lifecycle" for the full schema.

---

### Provider streaming regression

**Signature:** `provider_streaming` or `e2e_provider_streaming` test failures

**Artifacts:**
- `tests/fixtures/vcr/` (VCR cassettes)

**Replay:**
```bash
# VCR-backed
VCR_MODE=playback cargo test --test provider_streaming -- --nocapture

# E2E
cargo test --test e2e_provider_streaming -- --nocapture
```

**Remediation:**
1. Check if VCR cassettes are stale (model IDs changed, API format updated).
2. Verify `api_key: Some("vcr-playback".to_string())` in `StreamOptions`.
3. For URL mismatches: VCR uses strict URL matching; ensure model ID in test matches cassette.

---

### E2E TUI test failure

**Signature:** `e2e_tui` tests fail

**Artifacts:**
- E2E results directory

**Replay:**
```bash
cargo test --test e2e_tui -- --nocapture
```

**Remediation:**
1. TUI tests require tmux. Verify `tmux` is installed and accessible.
2. Set `PI_TEST_MODE=1` for deterministic rendering.
3. VCR cassettes provide provider responses; check cassette freshness.

---

### Flaky test (passes locally, fails on CI)

**Signature:** Inconsistent pass/fail across runs on the same commit.

**Replay:**
```bash
# Run with same parallelism as CI
cargo test --test <suite> -- --nocapture --test-threads=1

# Multiple runs to detect flakiness
for i in $(seq 1 5); do
    cargo test --test <suite> -- <test_name> --exact --nocapture || echo "FAIL on run $i"
done
```

**Remediation:**
1. Classify the flake per taxonomy (FLAKE-TIMING/ENV/NET/RES/EXT/LOGIC).
2. Add quarantine entry to `tests/suite_classification.toml`.
3. See `docs/testing-policy.md` "Flaky-Test Quarantine" for the full lifecycle.

---

## Parity Incident Response (DROPIN-162)

This section defines the operator workflow for parity regressions that threaten
strict drop-in claims.

### Incident triggers (open incident immediately)

- `tests/e2e_results/<ts>/triage_diff.json` has `status = "regression"` or
  `summary.regression_count > 0`.
- `tests/full_suite_gate/full_suite_verdict.json` shows a failed blocking gate
  affecting parity/test-log evidence (`e2e_log_contract`, `suite_classification`,
  `conformance_pass_rate`, `evidence_bundle`, or other blocking gate).
- `docs/evidence/dropin-certification-verdict.json` is missing, has
  `overall_verdict != CERTIFIED`, does not name the clean release source commit,
  or the final release ref includes any non-evidence descendant of that source
  when release messaging needs strict drop-in wording.
- CI parity suite gate fails (`PARITY GATE FAIL`) in `.github/workflows/ci.yml`.

### Severity and response targets

| Severity | Criteria | Response target |
|----------|----------|-----------------|
| `SEV-1` | Blocking parity regression on `main` or release cut path | Assign owner + post incident context within 30 minutes |
| `SEV-2` | New regression in PR/branch with no current release block | Assign owner + post context within 4 hours |
| `SEV-3` | Evidence/documentation drift without active behavior regression | Assign owner + post context within 1 business day |

### Evidence bundle for every parity incident

Collect and attach these artifacts to the incident bead and Agent Mail thread:

- `tests/e2e_results/<ts>/summary.json`
- `tests/e2e_results/<ts>/triage_diff.json`
- `tests/e2e_results/<ts>/replay_bundle.json`
- `tests/e2e_results/<ts>/failure_diagnostics_index.json`
- `tests/full_suite_gate/full_suite_verdict.json`
- `tests/full_suite_gate/full_suite_events.jsonl`
- `tests/full_suite_gate/full_suite_report.md`
- `tests/evidence_bundle/index.json`
- `docs/contracts/dropin-certification-contract.json`
- `docs/evidence/dropin-certification-verdict.json` (if present in the run)

### Response flow

1. Capture a reproducible baseline diff:
```bash
./scripts/e2e/run_all.sh --profile ci \
  --diff-from tests/e2e_results/<baseline-ts>/summary.json
```

2. Run gate replay commands for failing lanes:
```bash
cargo test --test ci_full_suite_gate -- full_suite_gate --nocapture --exact
cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact
cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact
```

3. Extract exact per-gate remediation commands from the verdict:
```bash
python3 - <<'PY'
import json
from pathlib import Path
p = Path("tests/full_suite_gate/full_suite_verdict.json")
if not p.exists():
    raise SystemExit("missing full_suite_verdict.json")
data = json.loads(p.read_text(encoding="utf-8"))
for gate in data.get("gates", []):
    if gate.get("status") == "fail":
        print(f"{gate['id']}: {gate.get('reproduce_command', 'N/A')}")
PY
```

4. Create/update the owning bead and notify the swarm in-thread (`thread_id = bead id`)
   with: failing gate IDs, `triage_diff.status`, top `ranked_diagnostics`, and
   one-command replay.

5. Apply fix and rerun:
```bash
./scripts/e2e/run_all.sh --rerun-from tests/e2e_results/<ts>/summary.json
cargo test --test ci_full_suite_gate -- full_suite_gate --nocapture --exact
```

6. Close only when all exit criteria are true:
   - `triage_diff.status` is not `regression`.
   - Blocking full-suite gates pass.
   - Drop-in wording guard is satisfied (`overall_verdict = CERTIFIED`) for release claims.
   - Bead + Agent Mail thread contain artifact links and final remediation note.

### Escalation path

- If unresolved beyond response target: escalate to maintainer in the same bead thread.
- If release train is active and `SEV-1` persists: freeze strict drop-in messaging until
  parity incident is closed.
- Use rollback mode (`CI_GATE_PROMOTION_MODE=rollback`) only as a short-lived emergency
  control; record rationale + expiry in the incident bead and restore `strict` after fix.

### PERF-3X Gate Incident Addendum (bd-3ar8v.6.4)

When the incident affects performance certification (not only drop-in wording), also
apply this fail-closed checklist:

1. Treat missing/stale PERF-3X artifacts as blocking failures:
   - `tests/full_suite_gate/perf3x_bead_coverage_audit.json`
   - `tests/full_suite_gate/practical_finish_checkpoint.json`
   - `tests/perf/reports/budget_summary.json`
   - `tests/perf/reports/perf_comparison.json`
   - `tests/perf/reports/stress_triage.json`
   - `tests/perf/reports/parameter_sweeps.json`
2. Attach `tests/full_suite_gate/certification_events.jsonl` plus perf event streams:
   - `tests/perf/reports/budget_events.jsonl`
   - `tests/perf/reports/perf_comparison_events.jsonl`
   - `tests/perf/reports/stress_events.jsonl`
   - `tests/perf/reports/parameter_sweeps_events.jsonl`
3. Use the log-query playbooks in `docs/qa-runbook.md` under
   **PERF-3X Regression Triage (bd-3ar8v.6.4)** for attribution and replay targeting.
4. Do not close the incident until detection, attribution, mitigation, and verification
   are all recorded in the bead thread with artifact links.

### PERF-3X signature: `parameter_sweeps_integrity` gate failure

**Signature:** `full_suite_verdict.json` contains gate `parameter_sweeps_integrity` with
`status = "fail"` and detail mentioning `parameter_sweeps.*` schema/readiness/source contract drift.

**Artifacts:**
- `tests/perf/reports/parameter_sweeps.json`
- `tests/perf/reports/parameter_sweeps_events.jsonl`
- `tests/perf/reports/phase1_matrix_validation.json`
- `tests/full_suite_gate/full_suite_verdict.json`

**Replay:**
```bash
rch exec -- cargo test --test release_evidence_gate -- \
  parameter_sweeps_contract_links_phase1_matrix_and_readiness --nocapture --exact
rch exec -- cargo test --test ci_full_suite_gate -- full_suite_gate --nocapture --exact
```

**Remediation:**
1. Enforce artifact schema `pi.perf.parameter_sweeps.v1`.
2. Enforce `source_identity` contract (`source_artifact = "phase1_matrix_validation"` and
   `source_artifact_path` references `phase1_matrix_validation.json`).
3. Enforce readiness invariants:
   - `status = ready` -> `ready_for_phase5 = true` and `blocking_reasons = []`
   - `status = blocked` -> `ready_for_phase5 = false` and non-empty `blocking_reasons`
4. Ensure `selected_defaults` are positive integers and `sweep_plan.dimensions` includes required knobs.
5. Re-run full-suite gate and re-attach updated `parameter_sweeps` artifact + event stream.

### PERF-3X signature: `practical_finish_checkpoint` readiness drift

**Signature:** gate `practical_finish_checkpoint` fails with detail like
`technical PERF-3X issue(s) still open` or `Fail-closed practical-finish source read error`.

**Artifacts:**
- `tests/full_suite_gate/practical_finish_checkpoint.json`
- `.beads/issues.jsonl` (or fallback `.beads/beads.base.jsonl`)
- `tests/full_suite_gate/full_suite_verdict.json`
- `tests/full_suite_gate/certification_events.jsonl`

**Replay:**
```bash
rch exec -- cargo test --test ci_full_suite_gate -- \
  practical_finish_report_fails_when_technical_open_issues_remain --nocapture --exact
rch exec -- cargo test --test release_readiness -- practical_finish_checkpoint_ -- --nocapture
rch exec -- cargo test --test ci_full_suite_gate -- full_suite_gate --nocapture --exact
```

**Remediation:**
1. Verify `practical_finish_checkpoint.json` schema is `pi.perf3x.practical_finish_checkpoint.v1`.
2. Ensure required contract fields are coherent:
   `status`, non-empty `detail`, `technical_completion_reached`, `residual_open_scope`,
   and count equality (`open_perf3x_count = technical_open_count + docs_or_report_open_count`).
3. Close or re-scope remaining technical PERF-3X issues; only docs/report residuals are allowed.
4. Re-run full-suite gate and attach refreshed checkpoint artifact + certification events before closure.

### FrankenNode claim signature: `claim_tier_order_drift`

**Signature:** claim-contract validation reports tier-order drift (or missing canonical
sequence) for:
- `TIER-1-EXTENSION-HOST-PARITY`
- `TIER-2-TARGETED-RUNTIME-PARITY`
- `TIER-3-FULL-NODE-BUN-REPLACEMENT`

**Artifacts:**
- `docs/franken-node-claim-gating-contract.json`
- `tests/full_suite_gate/franken_node_claim_verdict.json`
- `tests/full_suite_gate/practical_finish_checkpoint.json`

**Replay:**
```bash
rch exec -- cargo test --test franken_node_claim_contract -- \
  franken_node_claim_contract_declares_expected_tier_order -- --nocapture
rch exec -- cargo test --test release_evidence_gate -- \
  franken_node_claim_contract_is_present_and_valid --nocapture --exact
```

**Remediation:**
1. Restore canonical tier order in `claim_tiers` to Tier-1 -> Tier-2 -> Tier-3.
2. Ensure every tier still carries non-empty `required_evidence`,
   `allowed_claim_language`, and `forbidden_claim_language`.
3. Keep strict replacement gating fail-closed (`overall_verdict = CERTIFIED` required)
   and regenerate `franken_node_claim_verdict.json` before incident closure.

### FrankenNode kernel-boundary signature: `kernel_boundary_drift`

**Signature:** kernel-extraction boundary contract/report drift is detected in
manifest validation output, especially missing module ownership coverage,
duplicate ownership, or banned cross-boundary pair regressions.

**Artifacts:**
- `docs/franken-node-kernel-extraction-boundary-manifest.json`
- `tests/full_suite_gate/franken_node_kernel_boundary_drift_report.json`
- `tests/full_suite_gate/practical_finish_checkpoint.json`

**Replay:**
```bash
rch exec -- cargo test --test franken_node_kernel_extraction_boundary_manifest -- \
  kernel_boundary_manifest_ -- --nocapture
rch exec -- cargo test --test qa_docs_policy_validation -- \
  franken_node_mission_contract_tier_mapping_declares_required_checks_and_phase6_beads -- --nocapture
```

**Remediation:**
1. Ensure drift report checks remain present and fail-closed:
   `kernel_boundary.all_modules_mapped_or_deferred`,
   `kernel_boundary.no_duplicate_domain_ownership`,
   and `kernel_boundary.banned_cross_boundary_pairs_absent`.
2. Restore strict tier evidence linkage tokens in mission contract:
   `docs/franken-node-kernel-extraction-boundary-manifest.json` and
   `tests/full_suite_gate/franken_node_kernel_boundary_drift_report.json`.
3. Re-run the replay commands and attach refreshed artifacts before clearing the incident.

### FrankenNode compat harness signature: `node_runtime_unavailable_or_shimmed`

**Signature:** semantic compatibility harness fails or hard-skips because a real
Node runtime is unavailable, or Bun's `node` shim is incorrectly treated as
Node. Typical signals include `Node.js not found` and
`SKIP: generate_compatibility_matrix requires both Node.js and Bun`.

**Artifacts:**
- `tests/franken_node_compat_harness.rs`
- `tests/franken_node_compat/fixtures/`
- `tests/full_suite_gate/full_suite_verdict.json`

**Replay:**
```bash
rch exec -- cargo test --test franken_node_compat_harness -- \
  node_detection_rejects_bun_node_shim_when_present -- --nocapture
rch exec -- cargo test --test franken_node_compat_harness -- \
  generate_compatibility_matrix -- --nocapture
```

**Remediation:**
1. Keep `find_node()` and `is_real_node()` aligned with fail-closed detection:
   Bun's `/home/ubuntu/.bun/bin/node` shim must not pass as real Node.
2. Preserve deterministic skip diagnostics when Node/Bun are unavailable:
   `SKIP: Node.js not found on this machine`,
   `SKIP: Bun not found on this machine`, and
   `SKIP: generate_compatibility_matrix requires both Node.js and Bun`.
3. After runtime availability is corrected, re-run harness replay commands and
   attach refreshed verdict artifacts before clearing the incident.

---

## Evidence Artifact Interpretation

### summary.json

The primary run summary. Key fields:

| Field | Meaning |
|-------|---------|
| `failed_names` | List of failed E2E suite names |
| `failed_unit_names` | List of failed unit target names |
| `passed_suites` / `total_suites` | E2E suite pass rate |
| `replay_bundle.one_command_replay` | One-command to replay all failures |
| `triage_diff` | Baseline comparison (if `--diff-from` was used) |

### replay_bundle.json

Consolidated replay commands and environment context:

| Field | Meaning |
|-------|---------|
| `one_command_replay` | Single command to reproduce all failures |
| `environment.profile` | Run profile (quick/focused/ci/full) |
| `environment.vcr_mode` | VCR mode during the run |
| `environment.git_sha` | Git commit of the run |
| `failed_suites[].cargo_replay` | Per-suite cargo test command |
| `failed_suites[].targeted_replay` | Single-test cargo command |
| `failed_suites[].digest_path` | Path to per-suite failure digest |

### failure_digest.json

Per-suite failure analysis:

| Field | Meaning |
|-------|---------|
| `root_cause_class` | Classification: assertion_failure, timeout, panic, etc. |
| `impacted_scenario_ids` | List of failed test names |
| `first_failing_assertion` | Location and message of first failure |
| `remediation_pointer.replay_command` | Runner-level replay |
| `remediation_pointer.suite_replay_command` | Suite-level cargo test |
| `remediation_pointer.targeted_test_replay_command` | Single-test cargo test |

### triage_diff.json

Baseline comparison for regressions:

| Field | Meaning |
|-------|---------|
| `status` | `regression`, `stable`, or `known_failures_only` |
| `summary.regression_count` | New failures vs baseline |
| `ranked_diagnostics` | Severity-ranked list of changes |
| `recommended_commands.runner_repro_command` | Replay all problem targets |
| `recommended_commands.ranked_repro_commands` | Prioritized per-target commands |

---

## Shard Workflow

The CI runner supports sharding for parallel execution:

```bash
# Run shard 0 of 3 for E2E suites
./scripts/e2e/run_all.sh --profile ci --shard-kind suite --shard-index 0 --shard-total 3

# Run shard 1 of 4 for unit targets
./scripts/e2e/run_all.sh --profile ci --shard-kind unit --shard-index 1 --shard-total 4
```

Shard context is captured in:
- `environment.json`: `shard.kind`, `shard.index`, `shard.total`
- `summary.json`: same shard fields
- `replay_bundle.json`: `environment.shard_kind`, `shard_index`, `shard_total`

To replay a specific shard's failures, use the `--rerun-from` flag with that shard's
`summary.json`.
