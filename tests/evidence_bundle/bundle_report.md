# Unified CI Evidence Bundle

> Generated: 2026-08-04T07:06:47Z
> Git ref: 320475bc
> CI run: local-20260804T070647Z
> Verdict: **INSUFFICIENT**

## Summary

| Metric | Value |
|--------|-------|
| Total sections | 29 |
| Present | 28 |
| Missing | 0 |
| Invalid | 1 |
| Total artifacts | 2004 |
| Total size | 181049.4 KB |
| Required present | 12/12 |

## Conformance (6)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Extension conformance summary | PASS | 1 | 1058 B | `tests/ext_conformance/reports/conformance_summary.json` |
| Conformance baseline | PASS | 1 | 5170 B | `tests/ext_conformance/reports/conformance_baseline.json` |
| Conformance event log | PASS | 1 | 225456 B | `tests/ext_conformance/reports/conformance_events.jsonl` |
| Conformance report (Markdown) | PASS | 1 | 48656 B | `tests/ext_conformance/reports/CONFORMANCE_REPORT.md` |
| Regression gate verdict | PASS | 1 | 1658 B | `tests/ext_conformance/reports/regression_verdict.json` |
| Conformance trend data | PASS | 1 | 3075 B | `tests/ext_conformance/reports/conformance_trend.jsonl` |

## Diagnostics (8)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Must-pass gate verdict | PASS | 1 | 1236 B | `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json` |
| Must-pass gate event log | PASS | 1 | 61962 B | `tests/ext_conformance/reports/gate/must_pass_events.jsonl` |
| Per-extension failure dossiers | PASS | 34 | 140366 B | `tests/ext_conformance/reports/dossiers` |
| Health & regression delta report | PASS | 3 | 80504 B | `tests/ext_conformance/reports/health_delta` |
| Provider compatibility matrix | PASS | 753 | 375207 B | `tests/ext_conformance/reports/provider_compat` |
| Sharded extension matrix reports | PASS | 12 | 280518 B | `tests/ext_conformance/reports/sharded` |
| Extension journey report | PASS | 1 | 778 B | `tests/ext_conformance/reports/journeys/journey_report.json` |
| Auto-repair summary | PASS | 1 | 45400 B | `tests/ext_conformance/reports/auto_repair_summary.json` |

## E2e (1)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| E2E test results | PASS | 1176 | 183247322 B | `tests/e2e_results` |

## Quarantine (2)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Quarantine report | PASS | 1 | 499 B | `tests/quarantine_report.json` |
| Quarantine audit trail | PASS | 1 | 0 B | `tests/quarantine_audit.jsonl` |

## Performance (6)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Performance budget summary | PASS | 1 | 15361 B | `tests/perf/reports/budget_summary.json` |
| PERF-3X comparison report | PASS | 1 | 6350 B | `tests/perf/reports/perf_comparison.json` |
| PERF-3X parameter sweeps report | PASS | 1 | 1076 B | `tests/perf/reports/parameter_sweeps.json` |
| PERF-3X stress triage report | PASS | 1 | 4829 B | `tests/perf/reports/stress_triage.json` |
| Extension load-time benchmark | PASS | 1 | 17566 B | `tests/ext_conformance/reports/load_time_benchmark.json` |
| PERF-3X lineage coherence contract | WARN | 0 | 0 B | `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json | tests/ext_conformance/reports/conformance_summary.json | tests/perf/reports/stress_triage.json` |

## Security (2)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Security and licensing risk review | PASS | 1 | 86698 B | `tests/ext_conformance/artifacts/RISK_REVIEW.json` |
| Extension provenance verification | PASS | 1 | 143396 B | `tests/ext_conformance/artifacts/PROVENANCE_VERIFICATION.json` |

## Traceability (2)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Requirement-to-test traceability matrix | PASS | 1 | 94721 B | `docs/traceability_matrix.json` |
| High-value suite artifact inventory | PASS | 1 | 11209 B | `docs/evidence/high-value-suite-artifact-inventory.json` |

## Inventory (2)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Extension inventory | PASS | 1 | 89349 B | `tests/ext_conformance/reports/inventory.json` |
| Extension inclusion manifest | PASS | 4 | 405213 B | `tests/ext_conformance/reports/inclusion_manifest` |

## Missing / Invalid Sections

- **PERF-3X lineage coherence contract** (invalid): PERF-3X lineage span exceeds 14 days for run_id 'local-20260515T170218075Z' (oldest=2026-05-10 01:22:30 UTC, newest=2026-08-04 06:56:22 UTC)
  Path: `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json | tests/ext_conformance/reports/conformance_summary.json | tests/perf/reports/stress_triage.json`

