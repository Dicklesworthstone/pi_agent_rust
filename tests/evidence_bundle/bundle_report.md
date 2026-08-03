# Unified CI Evidence Bundle

> Generated: 2026-08-03T16:33:11Z
> Git ref: 5ff94dee
> CI run: 30806608383
> Verdict: **INSUFFICIENT**

## Summary

| Metric | Value |
|--------|-------|
| Total sections | 29 |
| Present | 18 |
| Missing | 10 |
| Invalid | 1 |
| Total artifacts | 93 |
| Total size | 792.1 KB |
| Required present | 12/12 |

## Conformance (6)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Extension conformance summary | PASS | 1 | 1058 B | `tests/ext_conformance/reports/conformance_summary.json` |
| Conformance baseline | PASS | 1 | 16247 B | `tests/ext_conformance/reports/conformance_baseline.json` |
| Conformance event log | MISS | 0 | 0 B | `tests/ext_conformance/reports/conformance_events.jsonl` |
| Conformance report (Markdown) | PASS | 1 | 48168 B | `tests/ext_conformance/reports/CONFORMANCE_REPORT.md` |
| Regression gate verdict | MISS | 0 | 0 B | `tests/ext_conformance/reports/regression_verdict.json` |
| Conformance trend data | MISS | 0 | 0 B | `tests/ext_conformance/reports/conformance_trend.jsonl` |

## Diagnostics (8)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Must-pass gate verdict | PASS | 1 | 1199 B | `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json` |
| Must-pass gate event log | PASS | 1 | 53754 B | `tests/ext_conformance/reports/gate/must_pass_events.jsonl` |
| Per-extension failure dossiers | MISS | 0 | 0 B | `tests/ext_conformance/reports/dossiers` |
| Health & regression delta report | PASS | 3 | 80204 B | `tests/ext_conformance/reports/health_delta` |
| Provider compatibility matrix | MISS | 0 | 0 B | `tests/ext_conformance/reports/provider_compat` |
| Sharded extension matrix reports | MISS | 0 | 0 B | `tests/ext_conformance/reports/sharded` |
| Extension journey report | PASS | 1 | 1185 B | `tests/ext_conformance/reports/journeys/journey_report.json` |
| Auto-repair summary | MISS | 0 | 0 B | `tests/ext_conformance/reports/auto_repair_summary.json` |

## E2e (1)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| E2E test results | PASS | 74 | 256474 B | `tests/e2e_results` |

## Quarantine (2)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Quarantine report | PASS | 1 | 499 B | `tests/quarantine_report.json` |
| Quarantine audit trail | PASS | 1 | 0 B | `tests/quarantine_audit.jsonl` |

## Performance (6)

| Section | Status | Files | Size | Path |
|---------|--------|-------|------|------|
| Performance budget summary | PASS | 1 | 4082 B | `tests/perf/reports/budget_summary.json` |
| PERF-3X comparison report | PASS | 1 | 6350 B | `tests/perf/reports/perf_comparison.json` |
| PERF-3X parameter sweeps report | PASS | 1 | 1076 B | `tests/perf/reports/parameter_sweeps.json` |
| PERF-3X stress triage report | PASS | 1 | 4797 B | `tests/perf/reports/stress_triage.json` |
| Extension load-time benchmark | MISS | 0 | 0 B | `tests/ext_conformance/reports/load_time_benchmark.json` |
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
| Extension inventory | MISS | 0 | 0 B | `tests/ext_conformance/reports/inventory.json` |
| Extension inclusion manifest | MISS | 0 | 0 B | `tests/ext_conformance/reports/inclusion_manifest` |

## Missing / Invalid Sections

- **Conformance event log** (missing): File not found
  Path: `tests/ext_conformance/reports/conformance_events.jsonl`
- **Regression gate verdict** (missing): File not found
  Path: `tests/ext_conformance/reports/regression_verdict.json`
- **Conformance trend data** (missing): File not found
  Path: `tests/ext_conformance/reports/conformance_trend.jsonl`
- **Per-extension failure dossiers** (missing): Directory not found
  Path: `tests/ext_conformance/reports/dossiers`
- **Provider compatibility matrix** (missing): Directory not found
  Path: `tests/ext_conformance/reports/provider_compat`
- **Sharded extension matrix reports** (missing): Directory not found
  Path: `tests/ext_conformance/reports/sharded`
- **Auto-repair summary** (missing): File not found
  Path: `tests/ext_conformance/reports/auto_repair_summary.json`
- **Extension load-time benchmark** (missing): File not found
  Path: `tests/ext_conformance/reports/load_time_benchmark.json`
- **Extension inventory** (missing): File not found
  Path: `tests/ext_conformance/reports/inventory.json`
- **Extension inclusion manifest** (missing): Directory not found
  Path: `tests/ext_conformance/reports/inclusion_manifest`
- **PERF-3X lineage coherence contract** (invalid): PERF-3X lineage span exceeds 14 days for run_id '30806608383' (oldest=2026-05-12 19:26:24 UTC, newest=2026-08-03 16:33:10 UTC)
  Path: `tests/ext_conformance/reports/gate/must_pass_gate_verdict.json | tests/ext_conformance/reports/conformance_summary.json | tests/perf/reports/stress_triage.json`

