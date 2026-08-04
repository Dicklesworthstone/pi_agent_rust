# Full Certification Report

> Generated: 2026-08-04T05:17:03Z
> Lane: **full**
> Verdict: **FAIL**

## Summary

| Metric | Value |
|--------|-------|
| Total gates | 20 |
| Passed | 17 |
| Failed | 3 |
| Warned | 0 |
| Skipped | 0 |
| Waived | 0 |
| Blocking | 12/14 |
| Can promote | NO |

## Gate Results

| Gate | Bead | Blocking | Status | Waived | Artifact |
|------|------|----------|--------|--------|----------|
| Non-mock unit compliance | bd-1f42.2.6 | YES | PASS | - | `docs/non-mock-rubric.json` |
| E2E log contract and transcripts | bd-1f42.3.6 | no | PASS | - | `tests/e2e_results` |
| Extension must-pass gate | bd-1f42.4.4 | YES | FAIL | - | `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json` |
| Extension provider compatibility matrix | bd-1f42.4.6 | no | PASS | - | `tests/ext_conformance/reports/provider_compat/provider_compat_report.json` |
| Unified evidence bundle | bd-1f42.6.8 | no | FAIL | - | `tests/evidence_bundle/index.json` |
| Cross-platform matrix validation | bd-1f42.6.7 | YES | PASS | - | `tests/cross_platform_reports/linux/platform_report.json` |
| Conformance regression gate | bd-1f42.4 | YES | PASS | - | `tests/ext_conformance/reports/regression_verdict.json` |
| Conformance pass rate >= 80% | bd-1f42.4 | YES | PASS | - | `tests/ext_conformance/reports/conformance_summary.json` |
| Suite classification guard | bd-1f42.6.1 | YES | PASS | - | `tests/suite_classification.toml` |
| Requirement traceability matrix | bd-1f42.6.4 | no | PASS | - | `docs/traceability_matrix.json` |
| Canonical E2E scenario matrix | bd-1f42.8.5.1 | no | PASS | - | `docs/e2e_scenario_matrix.json` |
| Provider gap test matrix coverage | bd-3uqg.11.11.5 | no | PASS | - | `docs/provider-gaps-test-matrix.json` |
| SEC-6.4 security compatibility conformance | bd-1a2cu | YES | PASS | - | `tests/full_suite_gate/sec_conformance_verdict.json` |
| PERF-3X bead-to-artifact coverage audit | bd-3ar8v.6.11 | YES | PASS | - | `tests/full_suite_gate/perf3x_bead_coverage_audit.json` |
| Practical-finish checkpoint (docs-only residual filter) | bd-3ar8v.6.9 | YES | FAIL | - | `tests/full_suite_gate/practical_finish_checkpoint.json` |
| Extension remediation backlog artifact integrity | bd-3ar8v.6.8 | YES | PASS | - | `tests/full_suite_gate/extension_remediation_backlog.json` |
| Opportunity matrix artifact integrity | bd-3ar8v.6.1 | YES | PASS | - | `tests/perf/reports/opportunity_matrix.json` |
| Parameter sweeps artifact integrity | bd-3ar8v.6.2 | YES | PASS | - | `tests/perf/reports/parameter_sweeps.json` |
| Conformance+stress lineage coherence | bd-3ar8v.6.3 | YES | PASS | - | `tests/ext_conformance/reports/conformance_summary.json` |
| Waiver lifecycle compliance | bd-1f42.8.8.1 | YES | PASS | - | `tests/full_suite_gate/waiver_audit.json` |

## Rerun Commands

| Lane | Command |
|------|--------|
| Preflight | `cargo test --test ci_full_suite_gate -- preflight_fast_fail --nocapture --exact` |
| Full | `cargo test --test ci_full_suite_gate -- full_certification --nocapture --exact` |

