# Extension Compatibility Validation Pack Summary

> Generated: 2026-02-08T02:01:00Z | Git: e8ea2a6e | Corpus: 223 extensions

## Current Snapshot

| Surface | Result | Source |
|---|---:|---|
| Extension conformance corpus | 205/223 pass (91.9%) | `tests/ext_conformance/reports/conformance/conformance_report.json` |
| Scenario conformance corpus | 24/25 pass (96.0%) | `tests/ext_conformance/reports/scenario_conformance.json` |
| Node/Bun runtime API matrix | 18/20 pass (90.0%) | `tests/ext_conformance/reports/parity/runtime_api_matrix.json` |

## Regression History

| Metric | Previous baseline (2026-02-07) | Current validation run (2026-02-08) | Delta |
|---|---:|---:|---:|
| Extension pass count | 187 | 205 | +18 |
| Extension fail count | 36 | 18 | -18 |
| Extension pass rate | 83.9% | 91.9% | +8.0 pp |
| Scenario pass count | 22 | 24 | +2 |
| Scenario fail count | 3 | 1 | -2 |
| Scenario pass rate | 88.0% | 96.0% | +8.0 pp |

## Extension Coverage by Tier

| Tier | Pass | Fail | Total | Pass rate |
|---|---:|---:|---:|---:|
| 1 | 38 | 0 | 38 | 100.0% |
| 2 | 83 | 4 | 87 | 95.4% |
| 3 | 79 | 11 | 90 | 87.8% |
| 4 | 1 | 2 | 3 | 33.3% |
| 5 | 4 | 1 | 5 | 80.0% |

## Remaining Failure Buckets (18 total)

| Bucket | Count | Examples |
|---|---:|---|
| Multi-file dependency (unsupported relative specifiers) | 4 | `community/qualisero-background-notify`, `community/qualisero-pi-agent-scip`, `community/qualisero-safe-git`, `npm/aliou-pi-processes` |
| Package module specifiers not supported in PiJS | 5 | `npm/pi-search-agent` (`openai`), `npm/pi-wakatime` (`adm-zip`), `npm/pi-web-access` (`linkedom`), `npm/qualisero-pi-agent-scip`, `third-party/qualisero-pi-agent-scip` |
| Host-read policy denials (outside extension root) | 4 | `npm/ogulcancelik-pi-sketch`, `npm/pi-interview`, `third-party/kcosr-pi-extensions`, `third-party/ogulcancelik-pi-sketch` |
| Runtime shape/load errors | 4 | `community/nicobailon-interview-tool`, `npm/aliou-pi-guardrails`, `npm/aliou-pi-toolchain`, `npm/marckrenn-pi-sub-core` |
| Test fixture / non-product artifact | 1 | `base_fixtures` |

## Runtime API Matrix Gaps

- Failing Bun APIs: `Bun.connect`, `Bun.listen`
- Matrix summary: Node `13/13` pass, Bun `5/7` pass
- Evidence includes linked unit targets, structured logs, and e2e workflow metadata in:
  `tests/ext_conformance/reports/parity/runtime_api_matrix.json`

## Structured Evidence Bundle

| Artifact | Path |
|---|---|
| Full per-extension conformance JSON | `tests/ext_conformance/reports/conformance/conformance_report.json` |
| Full per-extension conformance JSONL | `tests/ext_conformance/reports/conformance/conformance_events.jsonl` |
| Human-readable conformance report | `tests/ext_conformance/reports/conformance/conformance_report.md` |
| Scenario conformance summary | `tests/ext_conformance/reports/scenario_conformance.json` |
| Scenario conformance JSONL | `tests/ext_conformance/reports/scenario_conformance.jsonl` |
| Scenario triage summary | `tests/ext_conformance/reports/smoke_triage.json` |
| Inventory rollup | `tests/ext_conformance/reports/inventory.json` |
| Runtime API matrix | `tests/ext_conformance/reports/parity/runtime_api_matrix.json` |
| e2e workflow summary | `tests/e2e_results/20260208T015811Z/summary.json` |
| e2e evidence contract | `tests/e2e_results/20260208T015811Z/evidence_contract.json` |

## Reproducible Commands

```bash
# 223-extension conformance campaign
CARGO_TARGET_DIR=target-emeralddog cargo test \
  --test ext_conformance_generated conformance_full_report \
  --features ext-conformance -- --nocapture

# Scenario campaign
CARGO_TARGET_DIR=target-emeralddog cargo test \
  --test ext_conformance_scenarios --features ext-conformance \
  scenario_conformance_suite -- --nocapture

# Runtime API matrix
cargo test --test ext_conformance_matrix runtime_api_matrix_node_critical_entries_pass -- --nocapture
cargo test --test ext_conformance_matrix generate_runtime_api_matrix_report -- --nocapture

# Inventory rollup
python3 tests/ext_conformance/build_inventory.py

# e2e linkage evidence
./scripts/e2e/run_all.sh --profile quick --suite e2e_extension_registration --skip-lint --skip-unit
```
