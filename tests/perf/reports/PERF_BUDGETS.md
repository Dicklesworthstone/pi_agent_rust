# Performance Budgets

> Generated: 2026-08-22T01:20:02.027Z

> Run ID: not set

> Source commit: e64f414cf069cb769f1033ac660a135da56b1e27

> Strict mode: false

> Claim readiness: blocked

## Summary

| Metric | Value |
|---|---|
| Total budgets | 19 |
| CI-enforced | 14 |
| CI-enforced with data | 0 |
| CI-enforced FAIL | 0 |
| CI-enforced NO_DATA | 14 |
| PASS | 0 |
| FAIL | 0 |
| No data | 19 |

| Failing data contracts | 0 |

## Claim Readiness

Performance claims are blocked. Blocking reason codes:

- `budget_data_missing`
- `ci_budget_data_missing`
- `correlation_id_missing`
- `run_id_missing`
- `strict_mode_disabled`

## Startup

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `startup_version_p95` | p95 latency | <= | 100 ms | - | NO_DATA | Yes |
| `startup_full_agent_p95` | p95 latency | <= | 200 ms | - | NO_DATA | No |

## Extension

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `ext_cold_load_simple_p95` | p95 cold load time | <= | 5 ms | - | NO_DATA | Yes |
| `ext_cold_load_complex_p95` | p95 cold load time | <= | 50 ms | - | NO_DATA | No |
| `ext_load_60_total` | total load time (60 official extensions) | <= | 10000 ms | - | NO_DATA | No |

## Tool_call

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `tool_call_latency_mean` | mean per-call latency | <= | 200 us | - | NO_DATA | Yes |
| `tool_call_throughput_min` | minimum calls/sec | >= | 5000 calls/sec | - | NO_DATA | Yes |

## Event_dispatch

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `event_dispatch_p99` | p99 dispatch latency | <= | 5000 us | - | NO_DATA | No |

## Context_intelligence

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `context_graph_build_cold_p95` | p95 cold graph build latency | <= | 500 ms | - | NO_DATA | Yes |
| `context_graph_build_warm_p95` | p95 warm graph build latency | <= | 250 ms | - | NO_DATA | Yes |
| `context_incremental_update_p95` | p95 single-change rebuild latency | <= | 250 ms | - | NO_DATA | Yes |
| `context_planning_p95` | p95 planner latency | <= | 50 ms | - | NO_DATA | Yes |
| `context_bundle_serialization_p95` | p95 bundle serialization latency | <= | 25 ms | - | NO_DATA | Yes |
| `context_bundle_estimated_bytes_max` | bundle estimated size | <= | 262144 bytes | - | NO_DATA | Yes |

## Policy

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `policy_eval_p99` | p99 evaluation time | <= | 500 ns | - | NO_DATA | Yes |

## Memory

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `idle_memory_rss` | RSS at idle | <= | 50 MB | - | NO_DATA | Yes |
| `sustained_load_rss_growth` | RSS growth under 30s sustained load | <= | 5 percent | - | NO_DATA | No |

## Binary

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `binary_size_release` | release binary size | <= | 48 MB | - | NO_DATA | Yes |

## Protocol

| Budget | Metric | Comparison | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|---|
| `protocol_parse_p99` | p99 parse+validate time | <= | 50 us | - | NO_DATA | Yes |

## Failing Data Contracts

- Not evaluated: authoritative benchmark lineage is incomplete.

## Measurement Methodology

- **`startup_version_p95`**: hyperfine: `pi --version` (10 runs, 3 warmup)
- **`startup_full_agent_p95`**: hyperfine: `pi --print '.'` with full init (10 runs, 3 warmup)
- **`ext_cold_load_simple_p95`**: criterion: load_init_cold for simple single-file extensions (10 samples)
- **`ext_cold_load_complex_p95`**: criterion: load_init_cold for multi-registration extensions (10 samples)
- **`ext_load_60_total`**: conformance runner: sequential load of all 60 official extensions
- **`tool_call_latency_mean`**: pijs_workload: arithmetic mean across exactly 2000 iterations x 1 tool call, executable-path-verified perf profile
- **`tool_call_throughput_min`**: pijs_workload: aggregate throughput across exactly 2000 iterations x 10 tool calls, executable-path-verified perf profile
- **`event_dispatch_p99`**: criterion: event_hook dispatch for before_agent_start (100 samples)
- **`context_graph_build_cold_p95`**: criterion: semantic_context/graph_build_cold on large filesystem fixture
- **`context_graph_build_warm_p95`**: criterion: semantic_context/graph_build_warm on large filesystem fixture
- **`context_incremental_update_p95`**: criterion: semantic_context/incremental_update rebuild after one changed file
- **`context_planning_p95`**: criterion: semantic_context/planning on large graph fixture
- **`context_bundle_serialization_p95`**: criterion: semantic_context/bundle_serialization on large bundle fixture
- **`context_bundle_estimated_bytes_max`**: semantic_context budget artifact: estimated selected bundle bytes
- **`policy_eval_p99`**: criterion: ext_policy/evaluate with various modes and capabilities
- **`idle_memory_rss`**: sysinfo: measure RSS after startup, before any user input
- **`sustained_load_rss_growth`**: stress test: 15 extensions, 50 events/sec for 30 seconds
- **`binary_size_release`**: ls -la target/release/pi (stripped)
- **`protocol_parse_p99`**: criterion: ext_protocol/parse_and_validate for host_call and log messages

## CI Enforcement

CI-enforced budgets are checked on every PR. A budget violation blocks the PR from merging. Non-CI budgets are informational and checked in nightly runs.

```bash
# Run budget checks
cargo test --test perf_budgets -- --nocapture

# Generate full budget report
PI_GENERATE_PERF_BUDGET_REPORT=1 cargo test --test perf_budgets generate_budget_report -- --nocapture
```
