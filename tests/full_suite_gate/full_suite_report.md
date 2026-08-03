# Full-Suite CI Gate Report

> Generated: 2026-08-03T16:33:13Z
> Verdict: **FAIL**

## Summary

| Metric | Value |
|--------|-------|
| Total gates | 20 |
| Passed | 17 |
| Failed | 2 |
| Warned | 0 |
| Skipped | 1 |
| Blocking pass | 13/14 |

## Gate Results

| Gate | Bead | Blocking | Status | Artifact |
|------|------|----------|--------|----------|
| Non-mock unit compliance | bd-1f42.2.6 | YES | PASS | `docs/non-mock-rubric.json` |
| E2E log contract and transcripts | bd-1f42.3.6 | no | PASS | `tests/e2e_results` |
| Extension must-pass gate | bd-1f42.4.4 | YES | PASS | `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json` |
| Extension provider compatibility matrix | bd-1f42.4.6 | no | SKIP | `tests/ext_conformance/reports/provider_compat/provider_compat_report.json` |
| Unified evidence bundle | bd-1f42.6.8 | no | FAIL | `tests/evidence_bundle/index.json` |
| Cross-platform matrix validation | bd-1f42.6.7 | YES | PASS | `tests/cross_platform_reports/linux/platform_report.json` |
| Conformance regression gate | bd-1f42.4 | YES | FAIL | `tests/ext_conformance/reports/regression_verdict.json` |
| Conformance pass rate >= 80% | bd-1f42.4 | YES | PASS | `tests/ext_conformance/reports/conformance_summary.json` |
| Suite classification guard | bd-1f42.6.1 | YES | PASS | `tests/suite_classification.toml` |
| Requirement traceability matrix | bd-1f42.6.4 | no | PASS | `docs/traceability_matrix.json` |
| Canonical E2E scenario matrix | bd-1f42.8.5.1 | no | PASS | `docs/e2e_scenario_matrix.json` |
| Provider gap test matrix coverage | bd-3uqg.11.11.5 | no | PASS | `docs/provider-gaps-test-matrix.json` |
| SEC-6.4 security compatibility conformance | bd-1a2cu | YES | PASS | `tests/full_suite_gate/sec_conformance_verdict.json` |
| PERF-3X bead-to-artifact coverage audit | bd-3ar8v.6.11 | YES | PASS | `tests/full_suite_gate/perf3x_bead_coverage_audit.json` |
| Practical-finish checkpoint (docs-only residual filter) | bd-3ar8v.6.9 | YES | PASS | `tests/full_suite_gate/practical_finish_checkpoint.json` |
| Extension remediation backlog artifact integrity | bd-3ar8v.6.8 | YES | PASS | `tests/full_suite_gate/extension_remediation_backlog.json` |
| Opportunity matrix artifact integrity | bd-3ar8v.6.1 | YES | PASS | `tests/perf/reports/opportunity_matrix.json` |
| Parameter sweeps artifact integrity | bd-3ar8v.6.2 | YES | PASS | `tests/perf/reports/parameter_sweeps.json` |
| Conformance+stress lineage coherence | bd-3ar8v.6.3 | YES | PASS | `tests/ext_conformance/reports/conformance_summary.json` |
| Waiver lifecycle compliance | bd-1f42.8.8.1 | YES | PASS | `tests/full_suite_gate/waiver_audit.json` |

## Issues Requiring Attention

### Unified evidence bundle — FAIL

- **Bead:** bd-1f42.6.8
- **Detail:** summary.verdict = 'insufficient' (expected one of: ["complete", "partial"])
- **Artifact:** `tests/evidence_bundle/index.json`
- **Reproduce:**
  ```bash
  cargo test --test ci_evidence_bundle -- build_evidence_bundle --nocapture --exact
  ```

### Conformance regression gate — FAIL **(BLOCKING)**

- **Bead:** bd-1f42.4
- **Detail:** Artifact not found: tests/ext_conformance/reports/regression_verdict.json; fail-closed policy: blocking gates cannot remain in skip state
- **Artifact:** `tests/ext_conformance/reports/regression_verdict.json`
- **Reproduce:**
  ```bash
  cargo test --test conformance_regression_gate -- --nocapture
  ```

