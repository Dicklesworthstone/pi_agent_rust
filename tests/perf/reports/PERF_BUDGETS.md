# Performance Budgets

> Generated: 2026-02-06T06:21:55Z

## Summary

| Metric | Value |
|---|---|
| Total budgets | 13 |
| CI-enforced | 8 |
| PASS | 5 |
| FAIL | 0 |
| No data | 8 |

## Startup

| Budget | Metric | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|
| `startup_version_p95` | p95 latency | 100 ms | - | NO_DATA | Yes |
| `startup_full_agent_p95` | p95 latency | 200 ms | - | NO_DATA | No |

## Extension

| Budget | Metric | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|
| `ext_cold_load_simple_p95` | p95 cold load time | 5 ms | 0.8 | PASS | Yes |
| `ext_cold_load_complex_p95` | p95 cold load time | 50 ms | 0.6 | PASS | No |
| `ext_load_60_total` | total load time (60 official extensions) | 10000 ms | 6198.0 | PASS | No |

## Tool_call

| Budget | Metric | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|
| `tool_call_latency_p99` | p99 per-call latency | 200 us | 44 | PASS | Yes |
| `tool_call_throughput_min` | minimum calls/sec | 5000 calls/sec | 22883 | PASS | Yes |

## Event_dispatch

| Budget | Metric | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|
| `event_dispatch_p99` | p99 dispatch latency | 5000 us | - | NO_DATA | No |

## Policy

| Budget | Metric | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|
| `policy_eval_p99` | p99 evaluation time | 500 ns | - | NO_DATA | Yes |

## Memory

| Budget | Metric | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|
| `idle_memory_rss` | RSS at idle | 50 MB | - | NO_DATA | Yes |
| `sustained_load_rss_growth` | RSS growth under 30s sustained load | 5 percent | - | NO_DATA | No |

## Binary

| Budget | Metric | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|
| `binary_size_release` | release binary size | 20 MB | - | NO_DATA | Yes |

## Protocol

| Budget | Metric | Threshold | Actual | Status | CI |
|---|---|---|---|---|---|
| `protocol_parse_p99` | p99 parse+validate time | 50 us | - | NO_DATA | Yes |

## Measurement Methodology

- **`startup_version_p95`**: hyperfine: `pi --version` (10 runs, 3 warmup)
- **`startup_full_agent_p95`**: hyperfine: `pi --print '.'` with full init (10 runs, 3 warmup)
- **`ext_cold_load_simple_p95`**: criterion: load_init_cold for simple single-file extensions (10 samples)
- **`ext_cold_load_complex_p95`**: criterion: load_init_cold for multi-registration extensions (10 samples)
- **`ext_load_60_total`**: conformance runner: sequential load of all 60 official extensions
- **`tool_call_latency_p99`**: pijs_workload: 2000 iterations x 1 tool call, release build
- **`tool_call_throughput_min`**: pijs_workload: 2000 iterations x 10 tool calls, release build
- **`event_dispatch_p99`**: criterion: event_hook dispatch for before_agent_start (100 samples)
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
cargo test --test perf_budgets generate_budget_report -- --nocapture
```
