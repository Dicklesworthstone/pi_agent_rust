# QA Certification Dossier

> Generated: 2026-08-04T23:06:43.632Z
> Bead: bd-1f42.8.10
> Verdict: **PASS_WITH_RESIDUALS**

## Closure Question 1: Non-Mock Coverage

**Do we have full unit/integration coverage without mocks/fakes?**

The policy gates pass with quantified residuals. 302 test files classified (124 unit, 139 VCR, 39 E2E). Non-mock compliance gate passes (23 checks). Test double inventory: 267 entries across 21 modules. 7 allowlisted exceptions documented with owner and replacement plan. 3 tracked for active migration (Recording*/MockHostActions via bd-m9rk), 4 permanent with rationale.

Evidence:
- `docs/non-mock-rubric.json`
- `docs/test_double_inventory.json`
- `docs/testing-policy.md (Allowlisted Exceptions)`
- `tests/non_mock_compliance_gate.rs (23 tests)`

Residuals:
- 3 recording doubles tracked for migration (bd-m9rk)
- 132 high-risk entries in inventory (mostly extension_dispatcher inline stubs)
- model_selector_cycling uses DummyProvider (known, tracked)

## Closure Question 2: E2E Logging

**Do we have complete E2E integration scripts with detailed logging?**

The covered workflows have structured E2E evidence. 19/21 workflows covered (90%), 2 waived (live-only, requires credentials). 39 E2E test files classified. Structured logging: failure_digest.v1, failure_timeline.v1, evidence_contract.json, replay_bundle.v1. CI gate lanes: preflight fast-fail + full certification. Waiver lifecycle enforced. Replay bundles with environment context.

Evidence:
- `docs/e2e_scenario_matrix.json`
- `scripts/e2e/run_all.sh`
- `tests/ci_full_suite_gate.rs (106 tests)`
- `tests/e2e_replay_bundles.rs (10 tests pass)`
- `docs/qa-runbook.md`
- `docs/ci-operator-runbook.md`

Residuals:
- 2 waived workflows (live provider parity, require credentials)
- 3 CI gate failures and 0 skipped gates in the current full-suite verdict
- Evidence bundle current verdict: insufficient

## Suite Classification

| Suite | Files |
|-------|-------|
| Unit | 124 |
| VCR | 139 |
| E2E | 39 |
| **Total** | **302** |

## Allowlisted Exceptions

| Identifier | Owner | Status | Plan |
|------------|-------|--------|------|
| `MockHttpServer` | infra | accepted | Permanent: VCR cannot represent invalid UTF-8 bytes |
| `MockHttpRequest` | infra | accepted | Permanent: companion to MockHttpServer |
| `MockHttpResponse` | infra | accepted | Permanent: companion to MockHttpServer |
| `PackageCommandStubs` | infra | accepted | Permanent: real npm/git non-deterministic |
| `RecordingSession` | bd-m9rk | tracked | Replace with SessionHandle (most usages migrated) |
| `RecordingHostActions` | bd-m9rk | tracked | Evaluate agent-loop integration replacement |
| `MockHostActions` | bd-m9rk | tracked | Replace with real session-based dispatch |

## Residual Gaps

| ID | Severity | Follow-up | Description |
|-----|----------|-----------|-------------|
| ext_conformance_artifacts | low | bd-1f42.4.4 | Extension conformance is incomplete: 60/226 tested, 166 not exercised |
| evidence_bundle_artifact | low | bd-1f42.6.8 | Evidence bundle is not complete (current verdict: insufficient) |
| recording_doubles_cleanup | low | bd-m9rk | RecordingSession/RecordingHostActions/MockHostActions tracked for migration to real sessions |
| live_provider_parity | low | bd-1f42.8.5.3 | Live provider parity workflows waived (require live credentials) |

## Evidence Artifacts

| Artifact | Path | Exists |
|----------|------|--------|
| Suite classification | `tests/suite_classification.toml` | YES |
| Test double inventory | `docs/test_double_inventory.json` | YES |
| Non-mock rubric | `docs/non-mock-rubric.json` | YES |
| E2E scenario matrix | `docs/e2e_scenario_matrix.json` | YES |
| CI gate verdict | `tests/full_suite_gate/full_suite_verdict.json` | YES |
| Preflight verdict | `tests/full_suite_gate/preflight_verdict.json` | YES |
| Certification verdict | `tests/full_suite_gate/certification_verdict.json` | YES |
| Practical-finish checkpoint | `tests/full_suite_gate/practical_finish_checkpoint.json` | YES |
| Extension remediation backlog | `tests/full_suite_gate/extension_remediation_backlog.json` | YES |
| Parameter sweeps report | `tests/perf/reports/parameter_sweeps.json` | YES |
| Parameter sweeps events | `tests/perf/reports/parameter_sweeps_events.jsonl` | no |
| Opportunity matrix | `tests/perf/reports/opportunity_matrix.json` | YES |
| Waiver audit | `tests/full_suite_gate/waiver_audit.json` | YES |
| Replay bundle | `tests/full_suite_gate/replay_bundle.json` | YES |
| Testing policy | `docs/testing-policy.md` | YES |
| QA runbook | `docs/qa-runbook.md` | YES |
| CI operator runbook | `docs/ci-operator-runbook.md` | YES |

## Opportunity Matrix Contract

- Expected schema: `pi.perf.opportunity_matrix.v1`
- Artifact path: `tests/perf/reports/opportunity_matrix.json`
- Artifact present: `true`
- Contract valid: `true`
- Readiness: `blocked` / `NO_DECISION` (ready_for_phase5=`false`)
- Ranked opportunities: `0`
- Blocking reasons count: `1`

## Extension Remediation Backlog

- Total non-pass extensions: 0
- Actionable: 0
- Non-actionable: 0
- Artifact: `tests/full_suite_gate/extension_remediation_backlog.json`
- Markdown: `tests/full_suite_gate/extension_remediation_backlog.md`
