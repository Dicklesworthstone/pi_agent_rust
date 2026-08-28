# Performance Budgets (auto-generated)
> Generated: 2026-08-28T09:26:43.195245Z
> Git commit: e178a73d4145c25f09c845e65a8385a3684d7920
> Correlation ID: v0.3.0-fresh-measurement-20260828T092643Z
> Claim readiness: **blocked** (performance_claims_authorized=False)
> WARNING: **BLOCKED** - performance claims are NOT authorized in this revision.
## Per-budget results
| Budget | Category | CI-enforced | Threshold | Actual | Unit | Status | Source |
|---|---|---|---|---|---|---|---|
| startup_version_p95 | startup | yes | 100.0 | 4.199521562246709 | ms | **PASS** | cargo-target[0]://criterion/startup/version/warm/n |
| startup_full_agent_p95 | startup | no | 200.0 | 4.193748781142093 | ms | **PASS** | cargo-target[0]://criterion/startup/help/warm/new/ |
| ext_cold_load_simple_p95 | extension | yes | 5.0 | 11.903729893923613 | ms | **FAIL** | cargo-target[0]://criterion/ext_load_init/load_ini |
| ext_cold_load_complex_p95 | extension | no | 50.0 | 38297.549 | ms | **PASS** | file:///Users/jemanuel/projects/pi_agent_rust/test |
| ext_load_60_total | extension | no | 10000.0 | 6198.0 | ms | **PASS** | load_time_benchmark.json (sum of Rust load times) |
| tool_call_latency_mean | tool_call | yes | 200.0 | None | us | **FAIL** | no pijs_workload data |
| tool_call_throughput_min | tool_call | yes | 5000.0 | None | calls/sec | **FAIL** | no pijs_workload data |
| event_dispatch_p99 | event_dispatch | no | 5000.0 | 765.838 | us | **PASS** | file:///Users/jemanuel/projects/pi_agent_rust/test |
| context_graph_build_cold_p95 | context_intelligence | yes | 500.0 | 55.934902 | ms | **PASS** | cargo-target[0]://perf/context_intelligence/perf_b |
| context_graph_build_warm_p95 | context_intelligence | yes | 250.0 | 7.038828 | ms | **PASS** | cargo-target[0]://perf/context_intelligence/perf_b |
| context_incremental_update_p95 | context_intelligence | yes | 250.0 | 5.985682 | ms | **PASS** | cargo-target[0]://perf/context_intelligence/perf_b |
| context_planning_p95 | context_intelligence | yes | 50.0 | 2.900859 | ms | **PASS** | cargo-target[0]://perf/context_intelligence/perf_b |
| context_bundle_serialization_p95 | context_intelligence | yes | 25.0 | 0.582051 | ms | **PASS** | cargo-target[0]://perf/context_intelligence/perf_b |
| context_bundle_estimated_bytes_max | context_intelligence | yes | 262144.0 | 5120.0 | bytes | **PASS** | cargo-target[0]://perf/context_intelligence/perf_b |
| policy_eval_p99 | policy | yes | 500.0 | 74.67608857238598 | ns | **PASS** | criterion: ext_policy/evaluate (max) |
| idle_memory_rss | memory | yes | 50.0 | 8.0 | MB | **PASS** | file:///Users/jemanuel/projects/pi_agent_rust/test |
| sustained_load_rss_growth | memory | no | 5.0 | 0.2533103051237766 | percent | **PASS** | repo://tests/perf/reports/stress_triage.json |
| binary_size_release | binary | yes | 48.0 | 32.755 | MB | **PASS** | file:///Users/jemanuel/projects/pi_agent_rust/test |
| protocol_parse_p99 | protocol | yes | 50.0 | 3.8356520939478176 | us | **PASS** | criterion: ext_protocol/parse_and_validate (max) |

## Failures and missing data
- **ext_cold_load_simple_p95**: FAIL - cargo-target[0]://criterion/ext_load_init/load_init_cold/hello/new/estimates.json
- **tool_call_latency_mean**: FAIL - missing_measurement_data
- **tool_call_throughput_min**: FAIL - missing_measurement_data
